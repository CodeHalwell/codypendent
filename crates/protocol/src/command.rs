//! Commands: client requests for state changes.
//!
//! A [`Command`] carries an `idempotency_key` so a duplicate delivery produces
//! exactly one effect (the daemon records the first application and replays its
//! recorded result on a repeat — STEP 1.3). Commands request change; the daemon
//! decides, persists, and only then emits the resulting events.

use serde::{Deserialize, Serialize};

use crate::analytics::{AnalyticsBudgetRequest, AnalyticsExportRequest, AnalyticsQuery};
use crate::artifact::DataClassification;
use crate::automation::AutomationBindingRequest;
use crate::blackboard::{BlackboardItemDraft, BlackboardScope};
use crate::bundle::{BundleExportRequest, BundleImportRequest};
use crate::document::{DocumentEditLease, DocumentMutation, PublishTarget};
use crate::handshake::{ClientRole, Subscription};
use crate::ide::IdeContextUpdate;
use crate::ids::{
    ApprovalId, ArtifactId, AutomationBindingId, CheckpointId, CommandId, DocumentId, InboxEntryId,
    MemoryId, ModelId, PromptId, QuestionId, RunId, SessionId, WorkspaceId,
};
use crate::inbox::{InboxListQuery, InboxMutation};
use crate::input::InputEnvelope;
use crate::memory::MemoryScopeTier;
use crate::question::QuestionOutcome;
use crate::run::{AgentMode, ApprovalDecision, ApprovalScope, PromptDelivery};
use crate::session::{
    EditorActionContext, EditorNativeAction, PageCursor, SessionLifecycleAction, SessionSearchQuery,
};

pub use crate::session::SessionSummary;

/// An idempotent, optionally revision-guarded request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct Command {
    pub command_id: CommandId,
    /// Client-chosen key; the same key must never apply twice.
    pub idempotency_key: String,
    /// Optimistic-concurrency guard: apply only if the session is still at this
    /// revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    pub body: CommandBody,
}

/// Durable Remote UI plugin lifecycle status returned by daemon management
/// commands. Trust and execution authority remain daemon-owned; this is a
/// display-only projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct UiPluginLifecycleStatus {
    pub id: String,
    pub version: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_approval_receipt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_permission_diff: Option<String>,
}

/// The specific change a command requests. A wire enum: internally tagged with
/// an [`CommandBody::Unknown`] fallback so a command from a newer client
/// deserializes and is rejected structurally rather than crashing the peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum CommandBody {
    /// Browse daemon-owned knowledge in the scopes derived from this owned
    /// session. Clients cannot supply scope keys or read SQLite themselves.
    ReadKnowledge {
        session_id: SessionId,
        section: crate::knowledge::KnowledgeSection,
        #[serde(default)]
        offset: u32,
        #[serde(default)]
        limit: u32,
    },
    /// Verify and durably install a `.cody-ui.tgz` package disabled. The
    /// archive is base64 because JSON framing has no byte-string scalar and is
    /// bounded by the ordinary 16 MiB daemon frame limit.
    InstallUiPlugin {
        manifest_toml: String,
        artifact_base64: String,
        #[serde(default)]
        allow_unsigned: bool,
    },
    SmokeTestUiPlugin {
        plugin_id: String,
    },
    EnableUiPlugin {
        plugin_id: String,
        scope: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
    },
    ListUiPlugins,
    UpdateUiPlugin {
        plugin_id: String,
        manifest_toml: String,
        artifact_base64: String,
        #[serde(default)]
        allow_unsigned: bool,
    },
    ApproveUiPluginUpdate {
        plugin_id: String,
        approval_receipt: String,
    },
    RejectUiPluginUpdate {
        plugin_id: String,
        approval_receipt: String,
    },
    RevokeUiPlugin {
        plugin_id: String,
    },
    /// Incident-response operation: atomically remove publisher trust, revoke
    /// all signed Remote UI records from that publisher, and stop their workers.
    RemoveTrustedUiPublisher {
        publisher_id: String,
    },
    /// List sessions the daemon knows, newest-updated first (Adoption 11 S1).
    ListSessions {
        /// Restrict to one workspace; `None` lists all.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<WorkspaceId>,
        /// Hard cap on returned rows (the daemon also caps at 200).
        #[serde(default)]
        limit: Option<u32>,
    },
    /// Ranked, cursor-paged search over the Session Library.
    SearchSessions {
        query: SessionSearchQuery,
    },
    /// Read one stable page of session history.
    ReadSessionHistory {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<PageCursor>,
        #[serde(default)]
        limit: u32,
    },
    /// Rename, pin, archive, restore, delete, or export a session.
    MutateSessionLifecycle {
        session_id: SessionId,
        action: SessionLifecycleAction,
    },
    /// Start an ordinary attributable run from an editor-native action.
    RunEditorAction {
        session_id: SessionId,
        action: EditorNativeAction,
        context: EditorActionContext,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelId>,
    },
    /// Read one owner-authorized page of the durable inbox.
    ListInbox {
        #[serde(default)]
        query: InboxListQuery,
    },
    /// Acknowledge or dismiss a durable inbox entry.
    MutateInbox {
        mutation: InboxMutation,
    },
    /// Query measured usage and quality aggregates.
    QueryAnalytics {
        #[serde(default)]
        query: AnalyticsQuery,
    },
    /// Export a bounded analytics query as JSON or CSV.
    ExportAnalytics {
        request: AnalyticsExportRequest,
    },
    /// Create, read, list, update, or delete a trigger/schedule binding.
    ManageAutomationBinding {
        request: AutomationBindingRequest,
    },
    /// Create, read, list, update, or delete an owner-scoped spend/usage budget.
    ///
    /// Budgets are what make the threshold evaluator reachable: without a way
    /// to write `analytics_budgets` the evaluator, the alert type, the dedup
    /// key and the `BudgetWarning` inbox kind were all live code that nothing
    /// could ever trigger.
    ManageAnalyticsBudget {
        request: AnalyticsBudgetRequest,
    },
    /// Export a versioned, redacted session/support bundle.
    ExportBundle {
        request: BundleExportRequest,
    },
    /// Import a verified bundle without restoring credentials or authority.
    ImportBundle {
        request: BundleImportRequest,
    },
    /// Search workspace file paths with fuzzy matching (Adoption 11 M2).
    SearchWorkspaceFiles {
        repository: String,
        query: String,
        #[serde(default)]
        limit: Option<u32>,
    },
    CreateSession {
        workspace: WorkspaceId,
        title: String,
        /// The canonical filesystem root of the repository this session
        /// operates on, so the daemon can build its code graph on open (not
        /// only on the first run). `#[serde(default)]` keeps older clients
        /// (which send none) working.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        internal: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_session_id: Option<SessionId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_run_id: Option<RunId>,
    },
    /// Close an existing session without deleting its ledger or projections.
    /// The daemon accepts repeated closes as semantic no-ops.
    CloseSession {
        session_id: SessionId,
    },
    AttachSession {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_seen_sequence: Option<u64>,
        subscriptions: Vec<Subscription>,
        requested_role: ClientRole,
        /// The canonical filesystem root of the repository this session
        /// operates on, so the daemon can build its code graph on open (not
        /// only on the first run). `#[serde(default)]` keeps older clients
        /// (which send none) working.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository: Option<String>,
    },
    SubmitUserInput {
        session_id: SessionId,
        text: String,
        mode: AgentMode,
        /// The model to **pin** this continuation to (mid-conversation model
        /// switch). When the operator re-picks a model in the `/model` picker,
        /// the very next follow-up in the SAME session carries it here so the
        /// switch is instant — no restart, no new session. `Some(id)` runs this
        /// continuation on exactly that model AND makes it the session's current
        /// pin (a later follow-up that carries none inherits it via
        /// [`session_run_provenance`](crate) recovery). `None` is unchanged
        /// behavior: the continuation inherits the session's existing model from
        /// its originating `StartRun`. Mirrors
        /// [`StartRun.model`](CommandBody::StartRun::model): `#[serde(default)]`
        /// keeps an older client (which sends none) working — the daemon then
        /// resolves the model exactly as before this field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelId>,
        /// The full multimodal input this submission normalizes (voice v1,
        /// rubric 8): a typed [`InputEnvelope`] whose blocks may reference
        /// artifacts previously stored via
        /// [`PutArtifact`](CommandBody::PutArtifact) — e.g. an
        /// [`InputBlock::Audio`](crate::input::InputBlock::Audio) carrying a
        /// recorded voice note. When the envelope carries audio without a
        /// transcript, the daemon transcribes it (through its transcription
        /// seam, gated by the [`transcription_allowed`](crate::input::transcription_allowed)
        /// classification math) and the transcript text becomes the run input;
        /// the original audio stays linked to its transcript (the
        /// original-is-never-replaced invariant). Additive
        /// (`#[serde(default)]`): an older client omits it and `text` alone
        /// drives the run, exactly as before this field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        envelope: Option<InputEnvelope>,
    },
    StartRun {
        session_id: SessionId,
        objective: String,
        mode: AgentMode,
        /// The canonical filesystem root of the repository this run operates on.
        /// A per-user daemon can serve several checkouts over one socket, so the
        /// run — not the daemon's startup working directory — must decide which
        /// repository its context map and curated memories are attributed to
        /// (issue #6 item 1). `#[serde(default)]` keeps an older client (which
        /// sends none) working: the daemon then falls back to its own directory,
        /// exactly as before this field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository: Option<String>,
        /// The model to **pin** this run to (STEP MP2): when the operator picks
        /// a model in the `/model` picker, the run executes on exactly that
        /// model instead of the router's/resolver's choice. A pin overrides the
        /// daemon's *quality* judgment, never its *security* constraint — a
        /// pinned hosted model for classified data is refused (fail-closed),
        /// never silently run off-device (enforced in the executor). Mirrors
        /// [`repository`](CommandBody::StartRun::repository): `#[serde(default)]`
        /// keeps an older client (which sends none) working — the daemon then
        /// resolves the model exactly as before this field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelId>,
    },
    ResolveApproval {
        approval_id: ApprovalId,
        decision: ApprovalDecision,
        scope: ApprovalScope,
    },
    /// Resolve a parked question (adoption 03). Mirrors `ResolveApproval`:
    /// session-scoped, idempotent, revision-guarded.
    ResolveQuestion {
        question_id: QuestionId,
        outcome: QuestionOutcome,
    },
    CancelRun {
        run_id: RunId,
    },
    PauseRun {
        run_id: RunId,
    },
    ResumeRun {
        run_id: RunId,
    },
    QueueSteering {
        run_id: RunId,
        text: String,
    },
    /// Push the IDE's live context (active file, selection, open documents, and
    /// unsaved-buffer digests) for a session (Phase 3 STEP 3.4). Latest-wins and
    /// high-frequency (debounced ≥ 300 ms client-side), so the daemon stores it
    /// as a projection outside the event ledger rather than appending an event.
    UpdateIdeContext {
        session_id: SessionId,
        update: IdeContextUpdate,
    },
    /// Create a new collaborative document (Docs Studio, rubric #4 — before
    /// this command existed the Docs Studio browsed a set nothing could ever
    /// populate). Handled at the connection level like `MutateDocument`
    /// (documents live outside the session ledger): the daemon creates the
    /// document at revision 1 through its `DocumentCreator` seam — importing
    /// `initial_markdown` into typed blocks when present, else an empty block
    /// list — and replies
    /// [`DocumentCreated`](crate::envelope::Payload::DocumentCreated) carrying
    /// the new document id. An Observer is role-denied; a daemon assembled
    /// without a creator rejects it `document.transport-unavailable`.
    CreateDocument {
        /// The document title (non-empty; the daemon rejects a blank one).
        title: String,
        /// The scope to create the document in: `"repository"` (the default
        /// when absent — the document lives with the checkout),
        /// `"system"`, or `"organization:<id>"` (organization docs default to
        /// suggest-only agent collaboration). An unrecognized value is
        /// rejected `document.invalid-scope`, never guessed at.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        /// The canonical filesystem root of the repository a
        /// repository-scoped document belongs to. Mirrors
        /// [`StartRun.repository`](CommandBody::StartRun::repository):
        /// `#[serde(default)]` keeps an older client working — the daemon then
        /// falls back to its own startup root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository: Option<String>,
        /// Markdown to seed the document's blocks from (`docs new --from
        /// file.md`, the agent's `docs.create`). Imported lossily-but-
        /// reasonably at block granularity; absent creates an empty document.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initial_markdown: Option<String>,
    },
    /// Run the documentation staleness check (`/update-docs` glue, Phase 4
    /// STEP 4.6 finally wired): resolve every document's `{{ symbol:… }}`
    /// links against the code graph, persist them, diff for signature
    /// changes/disappearances, and file each finding as a Maintain-mode
    /// suggestion (never a direct edit). Handled at the connection level like
    /// `MutateDocument`; the daemon replies
    /// [`DocsCheckCompleted`](crate::envelope::Payload::DocsCheckCompleted)
    /// with the sweep's counts. An Observer is role-denied (the sweep files
    /// suggestions); a daemon assembled without a checker rejects it
    /// `document.transport-unavailable`.
    CheckDocuments {
        /// The canonical filesystem root of the repository whose code graph
        /// the links resolve against. Mirrors
        /// [`StartRun.repository`](CommandBody::StartRun::repository); absent
        /// falls back to the daemon's startup root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository: Option<String>,
        /// A session to surface the result into: when set and the sweep found
        /// anything stale, the daemon appends a `NoteAppended` to this
        /// session's ledger so the finding count reaches the active
        /// conversation. Absent, the counts ride only on the reply.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
    },
    /// Apply a semantic mutation to a collaborative document (Phase 4 STEP 4.3).
    ///
    /// Handled at the connection level (documents live outside the session
    /// ledger, so this never flows through the event write path): the daemon
    /// applies it onto the authoritative Loro document through its
    /// `DocumentMutator` seam — mode-gated by the document's scope (content
    /// edits become suggestions outside `Edit` mode), single-writer enforced
    /// via the edit-lease `require` pre-check — and fans the resulting
    /// `DocumentSync` out to the document's subscribers. An Observer is
    /// role-denied; a daemon assembled without a mutator rejects it
    /// `document.transport-unavailable`.
    MutateDocument {
        document_id: DocumentId,
        mutation: DocumentMutation,
    },
    /// Acquire (or renew) an edit lease over a document block-range before editing
    /// it (Phase 4 STEP 4.3 client transport). One writer per block-range: a
    /// whole-document lease (`block_id = None`) covers structural edits and
    /// conflicts with any block lease. The daemon replies
    /// [`DocumentLeaseGranted`](crate::envelope::Payload::DocumentLeaseGranted)
    /// with the minted lease id + expiry, or `CommandRejected` `document.range-leased`
    /// when a different writer holds an overlapping range. Like `MutateDocument`
    /// this is intercepted at the connection level (documents live outside the
    /// session ledger) rather than flowing through the event write path.
    AcquireDocumentLease {
        lease: DocumentEditLease,
        /// How long the lease is valid, in seconds; the daemon applies a default
        /// when absent. A re-acquire by the same holder renews the expiry in place.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ttl_seconds: Option<u64>,
    },
    /// Release a previously acquired document lease by its id (Phase 4 STEP 4.3).
    /// Idempotent — releasing an already-released or unknown lease is accepted as a
    /// no-op — so a client that loses the acknowledgement can retry safely.
    ReleaseDocumentLease {
        lease_id: String,
    },
    /// Publish a document's current revision to a Git target (Phase 4 STEP 4.4,
    /// closing the deferred "executing a `PublishPlan`" roadmap item).
    ///
    /// Handled at the connection level like `MutateDocument`/`StartWorkflow`
    /// (documents live outside the session ledger): the daemon computes the
    /// deterministic publish plan, then durably parks its approval — carrying
    /// the plan's target, changed files, and resulting Git action, shown
    /// verbatim on the approval card before any write — through the
    /// assembly's `DocumentPublisher` seam, and replies
    /// [`DocumentPublishRequested`](crate::envelope::Payload::DocumentPublishRequested)
    /// with the parked plan. Nothing is written until a human resolves the
    /// approval through the ordinary `ResolveApproval` command; a rejection
    /// executes nothing. Requires the `Controller` role; a daemon assembled
    /// without a publisher rejects it `document.transport-unavailable`.
    PublishDocument {
        document_id: DocumentId,
        target: PublishTarget,
    },
    /// Discover bounded named workflow metadata from the same sources used by StartWorkflow.
    /// Requires protocol 1.12 and the same role as starting a workflow.
    ListWorkflows {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository: Option<String>,
    },
    /// Start a durable workflow run from a compiled manifest (Phase 5 STEP 5.2).
    ///
    /// Carries the workflow **manifest YAML** (its content, not a path — the daemon
    /// never reads an arbitrary client-named file) and the typed `inputs` the
    /// manifest declares. Handled at the connection level like `MutateDocument` (a
    /// workflow run lives in its own durable store outside the session ledger): the
    /// daemon compiles the manifest, creates the run through its `WorkflowStarter`
    /// seam, and replies
    /// [`WorkflowRunStarted`](crate::envelope::Payload::WorkflowRunStarted) with the
    /// new run id — or `CommandRejected` when the manifest does not compile. A
    /// daemon assembled without a starter rejects it `workflow.transport-unavailable`.
    /// (Driving the created run is a later step; this command only makes runs
    /// durably creatable.)
    StartWorkflow {
        /// Optional precondition binding a new launcher to the selected parsed definition.
        /// A mismatch rejects before creating a run; legacy callers may omit it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_definition_digest: Option<String>,
        /// The workflow manifest YAML (the content of a `workflow.yaml`). Empty
        /// when [`workflow_id`](CommandBody::StartWorkflow::workflow_id) names a
        /// workflow the daemon resolves from its own sources instead.
        manifest: String,
        /// A named workflow to resolve from the daemon's sources (embedded
        /// built-ins, the user config directory, and the run repository's
        /// `.codypendent/workflows`) rather than shipping the manifest inline —
        /// the path `/fix-ci` takes (`repair-github-check`). Additive
        /// (`#[serde(default)]`): an older client omits it and ships `manifest`.
        /// When set, `manifest` is ignored and the daemon enforces the workflow
        /// registry's version-stability + shadowing rules at resolution.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow_id: Option<String>,
        /// The typed inputs the manifest declares, as JSON. Defaults to null when
        /// omitted (a workflow with no required inputs).
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        inputs: serde_json::Value,
        /// The canonical filesystem root of the repository this workflow's agent
        /// nodes operate on. A per-user daemon can serve several checkouts over
        /// one socket, so the run — not the daemon's startup working directory —
        /// must decide which repository its agent nodes' isolated worktrees are
        /// carved from (Phase 5 T5, fixing P5-D1). Mirrors
        /// [`StartRun.repository`](CommandBody::StartRun): `#[serde(default)]`
        /// keeps an older client (which sends none) working — the daemon then
        /// falls back to its own startup repository root, never a wandering
        /// `current_dir()` at node-execution time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository: Option<String>,
    },
    /// Pause a running durable workflow run (Phase 5 STEP 5.2 lifecycle command).
    ///
    /// Like `StartWorkflow`, handled at the connection level (a workflow run lives
    /// in its own durable store outside the session ledger): the daemon flips the
    /// run to `paused` through its `WorkflowLifecycle` seam so a live driver stops
    /// launching further nodes (cooperative pause — the in-flight wave finishes),
    /// and the run waits for a `ResumeWorkflow`. Controlling a run is a
    /// [`Controller`](crate::handshake::ClientRole::Controller) capability, so a
    /// lesser role is denied; a terminal run is rejected `workflow.illegal-transition`;
    /// a daemon without workflow transport rejects it `workflow.transport-unavailable`.
    PauseWorkflow {
        workflow_run_id: String,
    },
    /// Resume a paused durable workflow run (Phase 5 STEP 5.2). The daemon validates
    /// the run is paused and drives it onward from its ready frontier in the
    /// background, replying as soon as the resume is accepted. Only a paused run may
    /// be resumed (else `workflow.illegal-transition`); role/transport gating matches
    /// `PauseWorkflow`.
    ResumeWorkflow {
        workflow_run_id: String,
    },
    /// Re-drive a durable workflow run from a chosen node (Phase 5 STEP 5.2
    /// retry-from-node). The daemon resets that node and everything transitively
    /// downstream of it to `pending`, sets the run `running`, and drives in the
    /// background. An unknown `node_id` (or a graph that changed under the run) is
    /// rejected; role/transport gating matches `PauseWorkflow`.
    RetryWorkflowNode {
        workflow_run_id: String,
        /// The node id to re-drive from (its transitive dependents reset with it).
        node_id: String,
    },
    /// Cancel a durable workflow run (Phase 5 STEP 5.2 / T9 — the missing control:
    /// pause/resume/retry exist, cancel did not). Like `PauseWorkflow`, handled at
    /// the connection level and gated to the
    /// [`Controller`](crate::handshake::ClientRole::Controller) role. A cooperative
    /// drain (mirroring pause): the driver stops launching further nodes, any
    /// in-flight node's agent run is interrupted through the same cancellation
    /// machinery `CancelRun` uses, every still-`Pending` node becomes `Skipped`, and
    /// the run lands `Cancelled` — a **terminal** state (no resume from `Cancelled`;
    /// a later resume/pause is rejected `workflow.illegal-transition`). Idempotent on
    /// an already-cancelled run; a daemon without workflow transport rejects it
    /// `workflow.transport-unavailable`.
    CancelWorkflow {
        workflow_run_id: String,
    },
    /// Read a durable workflow run's observability snapshot (Phase 5 STEP 5.2 / T9):
    /// the run's current phase plus every node's full current view (state, attempt,
    /// measured cost, failure/block reason, budget warnings), in topological order.
    /// Like `ReadBlackboard`, intercepted at the connection level (a workflow run
    /// lives in its own durable store outside the session ledger) and served through
    /// the assembly's `WorkflowReader` seam; the daemon replies
    /// [`WorkflowRunSnapshot`](crate::envelope::Payload::WorkflowRunSnapshot). This
    /// is the catch-up baseline a client folds a `Subscription::Workflow` live stream
    /// on top of; reconstructed from the store, so a late subscriber after a restart
    /// still gets a truthful baseline. A **read** — any attached client (an Observer
    /// included) may issue it. An unknown run is rejected `workflow.run-not-found`; a
    /// daemon without workflow transport rejects it `workflow.transport-unavailable`.
    ReadWorkflowRun {
        workflow_run_id: String,
    },
    /// Draft a candidate for the evaluation-gated promotion pipeline (Phase 7
    /// STEP 7.5 — nothing promotes itself, ADR-010).
    ///
    /// Handled at the connection level like `StartWorkflow` (a promotion
    /// candidate lives in its own durable store outside the session ledger):
    /// the daemon creates a draft through its `PromotionGateway` seam and
    /// replies [`Payload::PromotionProposed`](crate::envelope::Payload::PromotionProposed)
    /// with the new candidate id — or `CommandRejected` when a synthesized
    /// candidate needs permission review, or the daemon has no promotion
    /// transport (`promotion.transport-unavailable`). `kind` is the wire name
    /// of an `ArtifactKind` (e.g. `"skill"`, `"router"`); an unrecognized kind
    /// is rejected rather than guessed at.
    ProposePromotion {
        kind: String,
        name: String,
        version: u32,
        #[serde(default)]
        requires_permission_review: bool,
    },
    /// Advance a candidate through the offline-regression / shadow / canary
    /// legs of the pipeline (Phase 7 STEP 7.5). `action` names exactly which
    /// transition to attempt; an illegal transition (wrong stage, or an
    /// unobserved canary trying to finish) is rejected verbatim as the
    /// underlying state-machine error, never silently coerced into success.
    /// Same connection-level handling and role gating as `ProposePromotion`.
    AdvancePromotion {
        candidate_id: String,
        action: PromotionAction,
    },
    /// **Approve and promote a candidate.** The human-approval gate
    /// (ADR-010, exit criterion 2): the daemon authenticates the acting party
    /// as `Actor::Human` from the connection's role — over this local-first
    /// socket, a `Controller`-role connection **is** the human operator (the
    /// same mapping `ResolveApproval` already uses for `resolved_by`) — and
    /// only a `Controller` may issue this command; every other role, and
    /// necessarily every non-human actor, is refused structurally before the
    /// promotion seam is ever invoked. No field on the wire lets a caller
    /// *supply* an actor — that would defeat the whole point of ADR-010.
    ApprovePromotion {
        candidate_id: String,
    },
    /// Manually roll back a promoted candidate to its predecessor version
    /// (Phase 7 STEP 7.5, exit criterion 4: reversible). Requires the
    /// `Controller` role like `ApprovePromotion`, and — unlike approval — the
    /// engine itself does not restrict rollback to a human actor (stopping a
    /// bad change needs no human, only promoting a good one does); the
    /// daemon still attributes the connection's mapped `Actor::Human` so a
    /// manual rollback is never confused with the system-attributed
    /// auto-rollback a canary regression produces on its own.
    RollbackPromotion {
        candidate_id: String,
    },
    /// Read a durable workflow run's blackboard (Phase 5 STEP 5.3): the typed
    /// artifacts its agents posted, optionally filtered by `kind`. Like
    /// `StartWorkflow`, intercepted at the connection level (a workflow run's board
    /// lives in its own durable store outside the session ledger): the daemon reads
    /// it through its `BlackboardReader` seam and replies
    /// [`BlackboardItems`](crate::envelope::Payload::BlackboardItems) with the
    /// matching [`BlackboardItemView`](crate::blackboard::BlackboardItemView)s. This
    /// is a **read** — any attached client, an Observer included, may issue it
    /// (there is no client-facing *post* command; only the workflow executor writes
    /// the board). A daemon assembled without a reader rejects it
    /// `workflow.transport-unavailable`.
    ReadBlackboard {
        workflow_run_id: String,
        /// A blackboard artifact kind to filter by (`finding`, `decision`, …), or
        /// all kinds when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        /// Include superseded revisions too; the default (`false`) returns only the
        /// live board (the "live-only" view the TUI shows).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        include_superseded: bool,
        /// Read a **repository task board** instead of a workflow run's board
        /// (Phase B kanban). When set, `workflow_run_id` is ignored: the daemon
        /// resolves the board to the synthetic run
        /// [`board_scope_id`](crate::blackboard::board_scope_id) names (an empty
        /// board for a repository never written to — a read creates nothing).
        /// Additive (`#[serde(default)]`): an older client omits it and reads a
        /// run board exactly as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        board_repository: Option<String>,
    },
    /// Post a blackboard artifact from a **client** (Phase B kanban — the write
    /// path the board and NL backlog need; the review's "deliberately no client
    /// post command" stance is revised here for board use). Handled at the
    /// connection level like `ReadBlackboard` (the board lives outside the
    /// session ledger) through the assembly's `BlackboardWriter` seam, and gated
    /// to the [`Controller`](crate::handshake::ClientRole::Controller) role — the
    /// local human operator; an agent still writes only through its
    /// `blackboard.*`/`task.*` tools, and an Observer stays read-only. The
    /// daemon builds the item's author from the issuing connection (never from a
    /// client-supplied identity) and replies
    /// [`BlackboardItemApplied`](crate::envelope::Payload::BlackboardItemApplied)
    /// with the stored item. A daemon without workflow transport rejects it
    /// `workflow.transport-unavailable`.
    PostBlackboardItem {
        /// The board to post onto: a workflow run's, or a repository's task
        /// board (created on first write).
        scope: BlackboardScope,
        /// The artifact to store (kind, payload, evidence, board fields).
        item: BlackboardItemDraft,
    },
    /// Update (supersede) a blackboard item from a client (Phase B kanban): a
    /// status/column move, a re-assignment, a re-order, or a payload edit. The
    /// same supersession discipline as an agent's correction — the store posts
    /// the replacement at the next revision and stamps the old row, never
    /// editing in place — so board history is preserved. Fields left `None`
    /// carry the old item's values forward. Role-gated and routed exactly like
    /// [`PostBlackboardItem`](CommandBody::PostBlackboardItem); replies
    /// [`BlackboardItemApplied`](crate::envelope::Payload::BlackboardItemApplied)
    /// with the replacement item.
    UpdateBlackboardItem {
        /// The board holding the item.
        scope: BlackboardScope,
        /// The live item to supersede. An already-superseded item is refused
        /// (`blackboard.already-superseded`), so concurrent moves never fork.
        item_id: String,
        /// The new column, when moving.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        /// The new assignee, when re-assigning.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assignee: Option<String>,
        /// The new within-column position, when re-ordering. When only `status`
        /// changes, the daemon appends to the end of the target column.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ordinal: Option<i64>,
        /// A replacement payload, when editing the card body.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
    },
    /// Read one page of a session's durable event history (fixing the >500-event
    /// catch-up gap: a `Catchup::Snapshot` carries no transcript, and no paged
    /// read existed). A **read** — any attached client, an Observer included,
    /// may issue it. The daemon replies
    /// [`SessionEventsPage`](crate::envelope::Payload::SessionEventsPage) with
    /// events `after_sequence < sequence <= after_sequence + limit` in ascending
    /// order; the client pages forward by passing the reply's `through` back as
    /// the next `after_sequence`. An unknown session is rejected
    /// `protocol.session-not-found`.
    ReadSessionEvents {
        session_id: SessionId,
        /// Return events strictly **after** this sequence (0 = from the start).
        #[serde(default, skip_serializing_if = "u64_is_zero")]
        after_sequence: u64,
        /// Maximum events in the page. 0 (or absent) asks for the server
        /// default; the server clamps any request to its own page ceiling.
        #[serde(default, skip_serializing_if = "u32_is_zero")]
        limit: u32,
    },
    /// Upload client-captured bytes into the daemon's content-addressed
    /// artifact store (voice v1, rubric 8): the client→daemon half of the
    /// multimodal input path. `bytes_base64` is base64 because JSON framing
    /// has no byte-string scalar (the `InstallUiPlugin` precedent) and is
    /// bounded by the ordinary 16 MiB daemon frame limit — comfortably enough
    /// for ~1 minute of 16 kHz mono WAV (~2 MB). `sensitivity` classifies the
    /// stored bytes (captured media should default to
    /// [`DEFAULT_MEDIA_CLASSIFICATION`](crate::input::DEFAULT_MEDIA_CLASSIFICATION),
    /// i.e. `Confidential`, so audio never leaves the device by accident);
    /// classification checks downstream always read the ref returned in
    /// [`ArtifactStored`](crate::envelope::Payload::ArtifactStored). Handled
    /// at the connection level (artifacts live outside the session ledger) and
    /// gated to the [`Controller`](crate::handshake::ClientRole::Controller)
    /// role — an upload is operator-supplied input, not an observer surface.
    /// Read one curated memory (Chapter 06's right to *inspect* what the
    /// fabric remembers). Handled at the connection level like `ReadBlackboard`
    /// — the memory store lives outside the session ledger — through the
    /// assembly's memory seam. A **read**: any handshaken client may issue it.
    ///
    /// `repository` names the checkout whose memories are in view; the daemon
    /// derives the repository identity from it with its own single source of
    /// truth (never from a client-supplied id) and answers only from the scopes
    /// that identity can see. A memory outside those scopes is refused
    /// **identically** to one that does not exist (`memory.not-found`), so this
    /// command is not an enumeration oracle for another checkout's memories.
    InspectMemory {
        id: MemoryId,
        repository: String,
    },
    /// Correct a memory (Chapter 06's right to *edit*). The store never edits in
    /// place: the correction is stored as a new record that `supersedes` the one
    /// it replaces, so the history of what was believed, and when, survives.
    /// Reserved to the [`Controller`](crate::handshake::ClientRole::Controller)
    /// role — an Observer may read a memory, never rewrite one. Refused
    /// `memory.not-found` for an absent, already-superseded, or out-of-scope id,
    /// all three identically. Replies
    /// [`Memory`](crate::envelope::Payload::Memory) with the NEW record.
    CorrectMemory {
        id: MemoryId,
        repository: String,
        statement: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        structured_value: Option<serde_json::Value>,
        confidence: f32,
    },
    /// Forget one memory (Chapter 06's right to *delete* — the one operation
    /// that really removes rows rather than superseding them). `Controller`-only
    /// and refused identically for absent and out-of-scope, like
    /// [`CorrectMemory`](CommandBody::CorrectMemory). Replies
    /// [`MemoryForgotten`](crate::envelope::Payload::MemoryForgotten) with a
    /// content-free audit of what was removed.
    ForgetMemory {
        id: MemoryId,
        repository: String,
    },
    /// Forget every memory in one of the caller's visible scopes — "forget what
    /// you know about this repository". `Controller`-only. The target is named
    /// by TIER, never by a scope key, so a bulk delete can only ever be aimed at
    /// a scope the caller can already see (see
    /// [`MemoryScopeTier`](crate::memory::MemoryScopeTier)).
    ForgetMemoryScope {
        repository: String,
        tier: MemoryScopeTier,
    },
    /// Fetch the content behind one of a memory's evidence refs — Chapter 06's
    /// "every retrieved memory opens its source", actually fetched instead of
    /// merely named. `evidence_index` is a position in
    /// [`MemoryView::evidence`](crate::memory::MemoryView::evidence). A **read**,
    /// gated by exactly the same scope check as `InspectMemory`: the memory is
    /// re-fetched under the caller's scopes before its evidence is opened, so a
    /// client cannot reach an artifact by naming a memory it may not see.
    OpenMemoryEvidence {
        id: MemoryId,
        repository: String,
        evidence_index: u32,
    },
    /// Submit the eval evidence a promotion's regression gate consumes (the
    /// socket replacement for a client writing `eval_suite_reports` into the
    /// daemon's database itself — see
    /// [`PromotionAction::RunRegression`]). `Controller`-only, like every other
    /// promotion verb: an automated CI-triggered `eval run --candidate-id` binds
    /// the same role, so this does not need `Actor::Human` the way
    /// `ApprovePromotion` does.
    ///
    /// The daemon re-derives the candidate's artifact kind/name/version from
    /// `promotion_candidates` and refuses a `report_json` that does not parse as
    /// a suite report or carries no cases at all — the verdict is computed by
    /// the daemon from the submitted cases, never taken as a caller-supplied
    /// pass/fail.
    SubmitEvalEvidence {
        candidate_id: String,
        /// The eval suite that ran. The regression gate consumes `core`.
        suite: String,
        /// The routing policy the suite ran under, or `daemon-default`.
        routing_policy: String,
        /// A serialized `codypendent_eval::SuiteReport`. Opaque here: protocol
        /// must not depend on `codypendent-eval`.
        report_json: String,
    },
    PutArtifact {
        /// IANA media type of the bytes, e.g. `audio/wav`.
        media_type: String,
        /// The raw bytes, base64-encoded (standard alphabet, with padding).
        bytes_base64: String,
        /// The stored occurrence's data classification.
        sensitivity: DataClassification,
    },
    /// Read one bounded range from an artifact after daemon-side ownership and
    /// integrity checks. The expected digest binds every chunk request to the
    /// reference originally observed by the client.
    ReadArtifact {
        artifact_id: ArtifactId,
        offset: u64,
        limit: u32,
        expected_sha256: String,
    },
    /// Fold the repository's code graph **now**, on demand, and report what the
    /// fold saw (`codypendent graph build`).
    ///
    /// Before this command the graph was built only as a side effect of opening
    /// a session or starting a run; there was no way to ask for it, and no way
    /// to find out why it was empty. `index rebuild` rebuilds the *retrieval*
    /// indexes and explicitly does not touch the graph, which read to users as
    /// though it did.
    ///
    /// Handled at the connection level like `ReadBlackboard` — the code graph
    /// lives outside the session ledger — through the assembly's code-graph
    /// seam. Gated to the [`Controller`](crate::handshake::ClientRole::Controller)
    /// role: a build clears and rewrites the repository's whole graph, which is
    /// a write, so an Observer may read the graph but never rebuild it.
    ///
    /// `repository` is a **path**: the daemon resolves it to the checkout with
    /// its own single source of truth and derives the repository identity from
    /// that, exactly as [`InspectMemory`](CommandBody::InspectMemory) does.
    /// A client cannot name a repository identity directly.
    /// The fold is unconditional. There is no "skip if already current" flag:
    /// a command a user reaches for because the graph looks wrong must not
    /// sometimes decline to do the thing its name promises.
    BuildCodeGraph {
        /// The directory to build from; resolved to its enclosing checkout.
        repository: String,
    },
    /// Describe the stored code graph for a repository, with no re-scan
    /// (`codypendent graph status`): counts, per-language and per-kind
    /// breakdowns, the revisions the graph is stamped at, and whether it is
    /// stale relative to the working tree.
    ///
    /// A **read** — any handshaken client, an Observer included, may issue it.
    /// `repository` is resolved exactly as
    /// [`BuildCodeGraph`](CommandBody::BuildCodeGraph)'s.
    ReadCodeGraphStatus {
        repository: String,
    },
    /// Ask whether a configured model can actually serve a run.
    ///
    /// The daemon owns `models.toml`, `auth.json` and `providers.toml`, so it
    /// is the only party that can answer without every client compiling a
    /// model registry and reading that disk itself. A client that is not on
    /// the daemon's machine could not answer at all.
    ProbeModel {
        /// The `models.toml` id to probe, or every configured model when
        /// absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelId>,
        /// Whether the daemon may reach the provider over the network.
        ///
        /// `false` — the default, and what a page full of rows should ask for
        /// — resolves credentials only. `true` is the per-row "Test": it costs
        /// a request to the provider and proves the model is listed. Never
        /// implied, because a client refreshing a list must not be able to
        /// make the daemon spend a request per model without saying so.
        #[serde(default)]
        network: bool,
    },
    /// List the graph's nodes and edges, filtered (`codypendent graph show`),
    /// so the graph is inspectable from a terminal rather than only through the
    /// TUI overlay.
    ///
    /// A **read**, like [`ReadCodeGraphStatus`](CommandBody::ReadCodeGraphStatus).
    /// The [`query`](CommandBody::ReadCodeGraph::query) narrows; it never
    /// widens: the repository gate is applied to every branch of it, including
    /// [`CodeGraphQuery::node_id`](crate::codegraph::CodeGraphQuery::node_id),
    /// and an id outside this repository is refused identically to one that
    /// does not exist.
    ReadCodeGraph {
        repository: String,
        #[serde(default)]
        query: crate::codegraph::CodeGraphQuery,
    },
    /// Restore a run's operating worktree to a recorded filesystem checkpoint
    /// (Adoption 04). Controller-only and **approval-gated**: the daemon parks
    /// a `ProposedAction::RestoreCheckpoint` approval and touches nothing
    /// until a human approves it; the restore itself is transactional (the
    /// current state is captured behind a private ref first and re-applied on
    /// any failure). Refused `checkpoint.run-active` while the run is live,
    /// `checkpoint.not-found` for an unknown id, and
    /// `checkpoint.worktree-missing` when the recorded worktree no longer
    /// exists on disk.
    RestoreCheckpoint {
        run_id: RunId,
        checkpoint: CheckpointId,
    },
    /// Fork a session at a recorded run-launch checkpoint (Phase 5 STEP 5.6,
    /// `ForkSession{checkpoint}` from Chapter 03; Adoption 05). The daemon
    /// copies the session's ledger up to (excluding) the checkpointed run —
    /// remapping run ids into a fresh id space — into a NEW session that
    /// records its fork origin, and replies
    /// [`SessionForked`](crate::envelope::Payload::SessionForked). The source
    /// session is never modified. Runs launched in the fork carve their
    /// worktrees from the checkpointed filesystem state, so the two branches
    /// are isolated while sharing all immutable pre-fork artifacts.
    /// Controller-only. `checkpoint` must be an ordinal-1 (run-launch)
    /// checkpoint of a run in `session_id`; a mid-run steering checkpoint is
    /// rejected `fork.mid-run-checkpoint`, an absent or foreign checkpoint
    /// `checkpoint.not-found` (identically — no oracle).
    ForkSession {
        session_id: SessionId,
        checkpoint: CheckpointId,
        /// The fork's title; absent derives `"<source title> (fork)"` with an
        /// opencode-style `#N` auto-increment on collision.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Queue a prompt on the session's server-side pending queue
    /// (Adoption 06). `delivery: Steer` targets the live run's next safe
    /// point; `Queue` waits for the session to go idle. Re-queuing text that
    /// is already queued updates the existing entry instead of duplicating
    /// it. Controller-only; blank text rejected `prompt-queue.empty`.
    QueuePrompt {
        session_id: SessionId,
        text: String,
        mode: AgentMode,
        delivery: PromptDelivery,
    },
    /// Edit a queued prompt in place (Tab-edit in the queue UI). Absent
    /// fields keep their values; an emptied `text` is rejected
    /// `prompt-queue.empty`; an unknown id `prompt-queue.not-found`.
    UpdateQueuedPrompt {
        session_id: SessionId,
        prompt_id: PromptId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delivery: Option<PromptDelivery>,
    },
    /// Promote a queued prompt to steer: delivery becomes `Steer` and the
    /// entry moves to the front (Enter on a selected queue row).
    PromoteQueuedPrompt {
        session_id: SessionId,
        prompt_id: PromptId,
    },
    /// Remove a queued prompt without running it.
    DeleteQueuedPrompt {
        session_id: SessionId,
        prompt_id: PromptId,
    },
    /// Run a user-initiated shell command as a transcript-recorded turn (Spec 20 Action 18).
    RunUserShell {
        session_id: SessionId,
        command: String,
    },
    /// Quick-add a curated memory directly from the composer (Spec 20 Action 20).
    /// Gated by the curator's secret and dedup filters; emitted to the session
    /// ledger as a `NoteAppended` event.
    RememberMemory {
        session_id: SessionId,
        text: String,
    },
    /// Search marketplace packages by query text.
    MarketplaceSearch {
        query: String,
        #[serde(default)]
        limit: Option<u32>,
    },
    /// Install a marketplace package (installed disabled by default).
    MarketplaceInstall {
        package_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        manifest_toml: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact_base64: Option<String>,
        #[serde(default)]
        allow_unsigned: bool,
    },
    /// Update an installed marketplace package.
    MarketplaceUpdate {
        package_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        manifest_toml: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact_base64: Option<String>,
        #[serde(default)]
        allow_unsigned: bool,
    },
    /// Enable an installed marketplace package.
    MarketplaceEnable {
        package_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
    },
    /// Disable an active marketplace package.
    MarketplaceDisable {
        package_id: String,
    },
    /// Revoke a marketplace package.
    MarketplaceRevoke {
        package_id: String,
        #[serde(default)]
        reason: String,
    },
    /// Declare an opaque secret reference.
    SecretDeclare {
        name: String,
        backend: String,
        locator: String,
        capability: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        organization_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository_id: Option<String>,
    },
    /// Issue a context-bound lease for a secret reference.
    SecretBind {
        reference_id: String,
        job_id: String,
        capability: String,
    },
    /// List declared secret references (metadata only, no material).
    SecretList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capability: Option<String>,
    },
    /// Revoke a declared secret reference.
    SecretRevoke {
        reference_id: String,
        #[serde(default)]
        reason: String,
    },
    // --- Milestone 6: Federation & Campaigns ---
    /// Establish or update the deterministic federated identity for a repository.
    EstablishFederatedIdentity {
        repository: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
    },
    /// Inspect a repository's outbound publication policy and effective settings.
    GetPublicationPolicy {
        repository: String,
    },
    /// Configure outbound graph publication policy for a repository.
    SetPublicationPolicy {
        repository: String,
        policy: crate::federated_graph::UpdatePublicationPolicyRequest,
    },
    /// Seal and publish an outbound batch of policy-approved graph facts.
    PublishGraphFacts {
        repository: String,
        #[serde(default)]
        idempotency_key: String,
    },
    /// Record a durable retraction / deletion tombstone for published facts.
    TombstoneGraphFacts {
        repository: String,
        subject_kind: String,
        subject_id: String,
        reason: String,
    },
    /// Query the shared federated graph across authorized repositories.
    QueryFederatedGraph {
        #[serde(default)]
        query: crate::federated_graph::FederatedGraphQuery,
    },
    /// Query cross-repository blast radius for a symbol or node.
    QueryBlastRadius {
        query: crate::federated_graph::BlastRadiusQuery,
    },
    /// Generate a coordinated API/schema migration plan across repositories.
    PlanMigration {
        query: crate::federated_graph::MigrationPlanQuery,
    },
    /// Suggest reviewer assignments with evidence from graph topology.
    SuggestReviewers {
        query: crate::federated_graph::ReviewerSuggestionQuery,
    },
    /// Create a multi-repository coordinated campaign.
    CreateCampaign {
        campaign: crate::federated_graph::CreateCampaignRequest,
    },
    /// Inspect the full execution state and children of a campaign.
    GetCampaign {
        campaign_id: String,
    },
    /// List campaigns with optional status filtering.
    ListCampaigns {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<crate::federated_graph::CampaignState>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    /// Execute or re-drive an enrolled multi-repository campaign.
    ExecuteCampaign {
        request: crate::federated_graph::ExecuteCampaignRequest,
    },
    /// Cancel an in-flight coordinated campaign.
    CancelCampaign {
        campaign_id: String,
    },
    #[serde(other)]
    Unknown,
}

/// One resource, already in the daemon's storage, that a [`CommandBody`] names
/// by id. See [`CommandBody::named_resources`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamedResource<'a> {
    Session(SessionId),
    Run(RunId),
    Approval(ApprovalId),
    Question(QuestionId),
    Document(DocumentId),
    Artifact(ArtifactId),
    InboxEntry(InboxEntryId),
    AutomationBinding(AutomationBindingId),
    /// An owner-scoped analytics budget. Borrowed `&str` rather than a UUID
    /// newtype because `analytics_budgets.id` is free-form `TEXT` (0043).
    Budget(&'a str),
    /// A document edit lease. A lease owns nothing itself: it is authorized
    /// through the document it is held over.
    DocumentLease(&'a str),
    /// A durable workflow run, or the `board:<repository>` task board that
    /// shares its id space (see [`board_scope_id`](crate::board_scope_id)).
    Workflow(std::borrow::Cow<'a, str>),
    /// A declared secret reference.
    SecretReference(std::borrow::Cow<'a, str>),
    /// A store with no per-row owner — every row in it is daemon-wide, so the
    /// only principal that can own it is the uid the daemon runs as.
    DaemonStore(DaemonStore),
}

/// A daemon-wide store a command addresses. Never on the wire: this is the
/// daemon's ownership axis for the stores whose rows have no owner of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStore {
    /// Curated memories (`InspectMemory`, `ForgetMemoryScope`, …).
    Memory,
    /// The evaluation-gated promotion pipeline and its evidence.
    Promotion,
    /// Installed Remote UI plugins — an arbitrary-code surface for the worker
    /// runtime, so it is gated exactly like the other two.
    UiPlugins,
    /// Installed marketplace packages.
    Marketplace,
    /// The syntax-layer code graph (`BuildCodeGraph`, `ReadCodeGraphStatus`,
    /// `ReadCodeGraph`) and federated graph / campaigns.
    CodeGraph,
    /// The daemon's model configuration and the credentials behind it
    /// (`ProbeModel`). Gated like the others precisely because a probe SPENDS
    /// those credentials against the provider: a foreign principal must not be
    /// able to make the daemon do that, nor to learn which models an operator
    /// has configured.
    Models,
    /// Metadata from the daemon's private workflow configuration directories.
    Workflows,
}

impl CommandBody {
    /// **Every** pre-existing resource this body names, in ONE exhaustive match.
    ///
    /// It lives here, in the crate that defines the enum, for one reason: this
    /// match has no wildcard arm, so a new `CommandBody` variant does not
    /// compile until somebody says what it names. (`CommandBody` is
    /// `#[non_exhaustive]`, so the same match in the daemon would need a
    /// wildcard and a new variant would silently classify as "names nothing" —
    /// which is precisely the failure this exists to prevent.)
    ///
    /// The daemon's socket server feeds this to its single ownership gate. Four
    /// consecutive reviews found the same defect: ownership was checked per
    /// command arm, and per-arm discipline leaks one arm at a time. The round-4
    /// leak was `PublishDocument`, which parked a Git write against another
    /// uid's document while both of its siblings re-derived ownership — and the
    /// difference between its success and a `document.not-found` was itself an
    /// enumeration oracle.
    ///
    /// An empty list means "names nothing that already exists": `CreateSession`
    /// and `CreateDocument` mint their own ids, `StartWorkflow` creates its run,
    /// `PutArtifact` stores fresh bytes.
    #[must_use]
    pub fn named_resources(&self) -> Vec<NamedResource<'_>> {
        match self {
            Self::ListWorkflows { .. } => vec![NamedResource::DaemonStore(DaemonStore::Workflows)],
            // The Remote UI plugin store is daemon-wide; `EnableUiPlugin` may
            // additionally scope a plugin to a session, which is a second,
            // per-row-owned id.
            Self::InstallUiPlugin { .. }
            | Self::SmokeTestUiPlugin { .. }
            | Self::ListUiPlugins
            | Self::UpdateUiPlugin { .. }
            | Self::ApproveUiPluginUpdate { .. }
            | Self::RejectUiPluginUpdate { .. }
            | Self::RevokeUiPlugin { .. }
            | Self::RemoveTrustedUiPublisher { .. } => {
                vec![NamedResource::DaemonStore(DaemonStore::UiPlugins)]
            }
            Self::EnableUiPlugin { session_id, .. } => {
                let mut named = vec![NamedResource::DaemonStore(DaemonStore::UiPlugins)];
                named.extend(session_id.map(NamedResource::Session));
                named
            }
            // `AttachSession` names a session and is deliberately absent: the
            // requested role binds to the connection BEFORE the attach is
            // evaluated (role bootstrap — a one-shot client asserts its role by
            // attaching to an id it may not own), and a rejected attach answers
            // `Payload::Error`, not `CommandRejected`. The daemon gates it
            // inside its attach handler with the same ownership check and the
            // same `protocol.session-not-found` answer.
            Self::AttachSession { .. } => Vec::new(),
            Self::ManageAutomationBinding { request } => match request {
                AutomationBindingRequest::Get { id }
                | AutomationBindingRequest::Update { id, .. }
                | AutomationBindingRequest::Delete { id } => {
                    vec![NamedResource::AutomationBinding(*id)]
                }
                AutomationBindingRequest::Create { .. }
                | AutomationBindingRequest::List { .. }
                | AutomationBindingRequest::Unknown => Vec::new(),
            },
            // Same shape as the binding arm above, and for the same reason: an
            // empty `Vec` here would make the ownership gate pass VACUOUSLY and
            // let any principal read or delete another principal's budget by id.
            Self::ManageAnalyticsBudget { request } => match request {
                AnalyticsBudgetRequest::Get { id }
                | AnalyticsBudgetRequest::Update { id, .. }
                | AnalyticsBudgetRequest::Delete { id } => {
                    vec![NamedResource::Budget(id.as_str())]
                }
                AnalyticsBudgetRequest::Create { .. }
                | AnalyticsBudgetRequest::List { .. }
                | AnalyticsBudgetRequest::Unknown => Vec::new(),
            },
            Self::MutateInbox { mutation } => match mutation {
                InboxMutation::Acknowledge { entry_id } | InboxMutation::Dismiss { entry_id } => {
                    vec![NamedResource::InboxEntry(*entry_id)]
                }
                InboxMutation::Unknown => Vec::new(),
            },
            Self::ListSessions { .. }
            | Self::SearchSessions { .. }
            | Self::ListInbox { .. }
            | Self::QueryAnalytics { .. }
            | Self::ExportAnalytics { .. }
            | Self::SearchWorkspaceFiles { .. }
            | Self::CreateSession { .. }
            | Self::CreateDocument { .. }
            | Self::StartWorkflow { .. }
            | Self::PutArtifact { .. }
            | Self::Unknown => Vec::new(),
            Self::ReadArtifact { artifact_id, .. } => vec![NamedResource::Artifact(*artifact_id)],
            Self::StartRun { session_id, .. }
            | Self::CloseSession { session_id }
            | Self::ReadSessionHistory { session_id, .. }
            | Self::MutateSessionLifecycle { session_id, .. }
            | Self::RunEditorAction { session_id, .. }
            | Self::SubmitUserInput { session_id, .. }
            | Self::RunUserShell { session_id, .. }
            | Self::RememberMemory { session_id, .. }
            | Self::UpdateIdeContext { session_id, .. }
            | Self::ReadSessionEvents { session_id, .. }
            | Self::ForkSession { session_id, .. }
            | Self::QueuePrompt { session_id, .. }
            | Self::UpdateQueuedPrompt { session_id, .. }
            | Self::PromoteQueuedPrompt { session_id, .. }
            | Self::DeleteQueuedPrompt { session_id, .. } => {
                vec![NamedResource::Session(*session_id)]
            }
            Self::ExportBundle { request } => request
                .source_session_ids
                .iter()
                .copied()
                .map(NamedResource::Session)
                .collect(),
            Self::ImportBundle { request } => {
                vec![NamedResource::Artifact(request.bundle.id)]
            }
            // The staleness sweep appends a note to whatever session it is
            // handed, so that session is a resource it names.
            Self::CheckDocuments { session_id, .. } => {
                session_id.map(NamedResource::Session).into_iter().collect()
            }
            Self::CancelRun { run_id }
            | Self::PauseRun { run_id }
            | Self::ResumeRun { run_id }
            | Self::QueueSteering { run_id, .. }
            | Self::RestoreCheckpoint { run_id, .. } => vec![NamedResource::Run(*run_id)],
            Self::ResolveApproval { approval_id, .. } => {
                vec![NamedResource::Approval(*approval_id)]
            }
            Self::ResolveQuestion { question_id, .. } => {
                vec![NamedResource::Question(*question_id)]
            }
            Self::MutateDocument { document_id, .. }
            | Self::PublishDocument { document_id, .. } => {
                vec![NamedResource::Document(*document_id)]
            }
            Self::AcquireDocumentLease { lease, .. } => {
                vec![NamedResource::Document(lease.document_id)]
            }
            Self::ReleaseDocumentLease { lease_id } => {
                vec![NamedResource::DocumentLease(lease_id.as_str())]
            }
            Self::PauseWorkflow { workflow_run_id }
            | Self::ResumeWorkflow { workflow_run_id }
            | Self::RetryWorkflowNode {
                workflow_run_id, ..
            }
            | Self::CancelWorkflow { workflow_run_id }
            | Self::ReadWorkflowRun { workflow_run_id } => {
                vec![NamedResource::Workflow(std::borrow::Cow::Borrowed(
                    workflow_run_id.as_str(),
                ))]
            }
            // A repository board read re-points at a synthetic board run the
            // daemon resolves, so what is named is the board when present and
            // the workflow run otherwise.
            Self::ReadBlackboard {
                workflow_run_id,
                board_repository,
                ..
            } => vec![NamedResource::Workflow(board_repository.as_deref().map_or(
                std::borrow::Cow::Borrowed(workflow_run_id.as_str()),
                |repository| std::borrow::Cow::Owned(crate::board_scope_id(repository)),
            ))],
            // Every board scope, not only `WorkflowRun`: a repository board is
            // owner-checked too. A scope from a newer client names nothing here
            // and is rejected structurally where it is lowered.
            Self::PostBlackboardItem { scope, .. } | Self::UpdateBlackboardItem { scope, .. } => {
                board_scope_resource(scope).into_iter().collect()
            }
            Self::ReadKnowledge { session_id, .. } => vec![
                NamedResource::Session(*session_id),
                NamedResource::DaemonStore(DaemonStore::Memory),
            ],
            Self::InspectMemory { .. }
            | Self::CorrectMemory { .. }
            | Self::ForgetMemory { .. }
            | Self::ForgetMemoryScope { .. }
            | Self::OpenMemoryEvidence { .. } => {
                vec![NamedResource::DaemonStore(DaemonStore::Memory)]
            }
            Self::ProposePromotion { .. }
            | Self::AdvancePromotion { .. }
            | Self::ApprovePromotion { .. }
            | Self::RollbackPromotion { .. }
            | Self::SubmitEvalEvidence { .. } => {
                vec![NamedResource::DaemonStore(DaemonStore::Promotion)]
            }
            // The code graph is addressed by a repository PATH, never by a row
            // id, so there is no per-row owner to check here. The store gate
            // below answers "may this principal address the graph at all"; the
            // repository gate — which is what stops one checkout's query from
            // reading another's — lives inside the seam, where the rows are
            // fetched, for both the list and the by-id path.
            Self::BuildCodeGraph { .. }
            | Self::ReadCodeGraphStatus { .. }
            | Self::ReadCodeGraph { .. } => {
                vec![NamedResource::DaemonStore(DaemonStore::CodeGraph)]
            }
            Self::ProbeModel { .. } => {
                vec![NamedResource::DaemonStore(DaemonStore::Models)]
            }
            Self::MarketplaceSearch { .. }
            | Self::MarketplaceInstall { .. }
            | Self::MarketplaceUpdate { .. }
            | Self::MarketplaceDisable { .. }
            | Self::MarketplaceRevoke { .. } => {
                vec![NamedResource::DaemonStore(DaemonStore::Marketplace)]
            }
            Self::MarketplaceEnable { session_id, .. } => {
                let mut named = vec![NamedResource::DaemonStore(DaemonStore::Marketplace)];
                named.extend(session_id.map(NamedResource::Session));
                named
            }
            Self::SecretDeclare { .. } | Self::SecretList { .. } => Vec::new(),
            Self::SecretBind { reference_id, .. } | Self::SecretRevoke { reference_id, .. } => {
                vec![NamedResource::SecretReference(std::borrow::Cow::Borrowed(
                    reference_id.as_str(),
                ))]
            }
            Self::EstablishFederatedIdentity { .. }
            | Self::GetPublicationPolicy { .. }
            | Self::SetPublicationPolicy { .. }
            | Self::PublishGraphFacts { .. }
            | Self::TombstoneGraphFacts { .. }
            | Self::QueryFederatedGraph { .. }
            | Self::QueryBlastRadius { .. }
            | Self::PlanMigration { .. }
            | Self::SuggestReviewers { .. }
            | Self::CreateCampaign { .. }
            | Self::GetCampaign { .. }
            | Self::ListCampaigns { .. }
            | Self::CancelCampaign { .. } => {
                vec![NamedResource::DaemonStore(DaemonStore::CodeGraph)]
            }
            Self::ExecuteCampaign { request } => {
                vec![
                    NamedResource::DaemonStore(DaemonStore::CodeGraph),
                    NamedResource::Workflow(std::borrow::Cow::Borrowed(
                        request.campaign_id.as_str(),
                    )),
                ]
            }
        }
    }
}

/// The workflow-run id a board scope is authorized against, or `None` for a
/// scope this daemon does not understand.
fn board_scope_resource(scope: &BlackboardScope) -> Option<NamedResource<'_>> {
    match scope {
        BlackboardScope::WorkflowRun { workflow_run_id } => Some(NamedResource::Workflow(
            std::borrow::Cow::Borrowed(workflow_run_id.as_str()),
        )),
        BlackboardScope::RepositoryBoard { repository } => Some(NamedResource::Workflow(
            std::borrow::Cow::Owned(crate::board_scope_id(repository)),
        )),
        _ => None,
    }
}

/// `skip_serializing_if` helpers for the paged-history defaults: a zero is the
/// field's default, so it is omitted on the wire (an older peer's exact shape).
fn u64_is_zero(value: &u64) -> bool {
    *value == 0
}
fn u32_is_zero(value: &u32) -> bool {
    *value == 0
}

/// One legal state-machine transition to attempt via `AdvancePromotion`
/// (Phase 7 STEP 7.5). Mirrors `codypendent_eval::promote::Candidate`'s
/// methods exactly. Regression and canary verdicts are computed from durable
/// evidence by the daemon, never supplied as client booleans.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum PromotionAction {
    /// Evaluate the latest durable core SuiteReport bound to this candidate.
    RunRegression,
    /// Record the Controller's permission review before evaluation.
    ReviewPermissions,
    /// Begin the shadow run.
    StartShadow,
    /// Begin the limited canary.
    StartCanary,
    /// Measure the candidate's canary slice and derive the verdict.
    ///
    /// **Carries no payload, deliberately.** A previous revision was
    /// `ObserveCanary { metrics: CanaryMetrics }`: the verdict was derived
    /// server-side, but the *numbers* it was derived from came from the
    /// request, and the shipped CLI asked a human to type all five. That made
    /// `MIN_CANARY_SAMPLES` satisfiable by typing `500`. The daemon now reads
    /// the numbers out of its own recorded executions
    /// (`execution_observations`) instead; there is no field a caller could
    /// use to assert a sample count, an error rate, or a latency.
    ObserveCanary,
    /// Finish the canary and assemble the comparison. Refused until the
    /// server has accumulated the required measured sample population.
    FinishCanary,
    #[serde(other)]
    Unknown,
}

/// Canary metrics **measured by the daemon** and persisted as promotion
/// evidence (`promotion_canary_evidence.metrics_json`). Rates are basis points
/// (0..=10,000); latency is milliseconds.
///
/// This type is never accepted from a client. It used to ride inside
/// [`PromotionAction::ObserveCanary`], which is exactly how a promotion gate
/// meant to catch a regression became passable by typing numbers into a CLI.
/// It survives as the shape the server's own measurement serializes to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct CanaryMetrics {
    pub sample_count: u64,
    pub error_rate_bps: u16,
    pub baseline_error_rate_bps: u16,
    pub p95_latency_ms: u64,
    pub baseline_p95_latency_ms: u64,
}

/// Wire match item for workspace file fuzzy search (Adoption 11 M2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct FileMatchWire {
    pub path: String,
    pub indices: Vec<u32>,
    pub score: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(body: CommandBody) {
        let command = Command {
            command_id: CommandId::new(),
            idempotency_key: "idem-1".to_string(),
            expected_revision: Some(7),
            body,
        };
        let json = serde_json::to_string(&command).expect("serialize");
        let parsed: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(command, parsed);
    }

    /// `ObserveCanary` lost its `metrics` payload. Two things follow, and both
    /// are asserted rather than assumed:
    ///
    /// 1. A current client emits no numbers at all — there is no field left to
    ///    assert a sample count with.
    /// 2. An OLDER client's payload, which still carries `metrics`, keeps
    ///    parsing: an internally-tagged unit variant ignores the rest of the
    ///    map. Its numbers are ignored rather than honoured, so an old
    ///    `codypendent promote --step observe-canary --sample-count 500` gets a
    ///    server measurement (or a refusal), never its own typed evidence.
    #[test]
    fn observe_canary_carries_no_numbers_and_ignores_an_older_clients_metrics() {
        let json = serde_json::to_string(&PromotionAction::ObserveCanary).expect("serialize");
        assert_eq!(json, r#"{"type":"ObserveCanary"}"#);

        let legacy = r#"{"type":"ObserveCanary","metrics":{"sample_count":500,
            "error_rate_bps":40,"baseline_error_rate_bps":35,"p95_latency_ms":900,
            "baseline_p95_latency_ms":950}}"#;
        let parsed: PromotionAction = serde_json::from_str(legacy).expect("an older client parses");
        assert_eq!(
            parsed,
            PromotionAction::ObserveCanary,
            "the typed 500 samples are dropped, not carried into the gate"
        );
    }

    #[test]
    fn start_run_repository_is_omitted_when_absent_and_reparses_to_none() {
        // The per-run repository (issue #6 item 1) is optional on the wire: a
        // client that sends none produces JSON without the key, and such a
        // payload (also what an older client emits) parses back to `None` so the
        // daemon falls back to its own directory.
        let body = CommandBody::StartRun {
            session_id: SessionId::new(),
            objective: "diagnose".to_string(),
            mode: AgentMode::Build,
            repository: None,
            model: None,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(
            !json.contains("repository"),
            "an absent repository is skipped on the wire: {json}"
        );
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body, "a payload without the key defaults to None");
    }

    #[test]
    fn start_run_pins_a_model_when_present_and_omits_it_when_absent() {
        // A pinned model (STEP MP2) mirrors `repository`: present on the wire
        // when set and reparsed exactly, absent (skipped) when None so an older
        // client — and any run that lets the daemon resolve the model — keeps
        // working unchanged.
        let pinned = CommandBody::StartRun {
            session_id: SessionId::new(),
            objective: "diagnose".to_string(),
            mode: AgentMode::Build,
            repository: None,
            model: Some(ModelId("claude-sonnet-5".to_string())),
        };
        let json = serde_json::to_string(&pinned).expect("serialize");
        assert!(
            json.contains("claude-sonnet-5"),
            "a pinned model is on the wire: {json}"
        );
        assert_eq!(
            serde_json::from_str::<CommandBody>(&json).expect("deserialize"),
            pinned
        );

        let unpinned = CommandBody::StartRun {
            session_id: SessionId::new(),
            objective: "diagnose".to_string(),
            mode: AgentMode::Build,
            repository: None,
            model: None,
        };
        let json = serde_json::to_string(&unpinned).expect("serialize");
        assert!(
            !json.contains("model"),
            "an absent pinned model is skipped on the wire: {json}"
        );
        assert_eq!(
            serde_json::from_str::<CommandBody>(&json).expect("deserialize"),
            unpinned,
            "a payload without the key defaults to None"
        );
    }

    #[test]
    fn submit_user_input_pins_a_model_when_present_and_omits_it_when_absent() {
        // A mid-conversation model switch (the re-pick the TUI sends on the very
        // next follow-up) mirrors `StartRun`'s pin: present on the wire when set
        // and reparsed exactly, absent (skipped) when None so an older client —
        // and any continuation that lets the daemon inherit the session's model —
        // keeps working unchanged.
        let pinned = CommandBody::SubmitUserInput {
            session_id: SessionId::new(),
            text: "switch to the big model".to_string(),
            mode: AgentMode::Build,
            model: Some(ModelId("claude-sonnet-5".to_string())),
            envelope: None,
        };
        let json = serde_json::to_string(&pinned).expect("serialize");
        assert!(
            json.contains("claude-sonnet-5"),
            "a re-pinned model is on the wire: {json}"
        );
        assert_eq!(
            serde_json::from_str::<CommandBody>(&json).expect("deserialize"),
            pinned
        );

        let unpinned = CommandBody::SubmitUserInput {
            session_id: SessionId::new(),
            text: "keep going".to_string(),
            mode: AgentMode::Build,
            model: None,
            envelope: None,
        };
        let json = serde_json::to_string(&unpinned).expect("serialize");
        assert!(
            !json.contains("model"),
            "an absent pinned model is skipped on the wire: {json}"
        );
        assert!(
            !json.contains("envelope"),
            "an absent envelope is skipped on the wire (an older client's exact bytes): {json}"
        );
        assert_eq!(
            serde_json::from_str::<CommandBody>(&json).expect("deserialize"),
            unpinned,
            "a payload without the key defaults to None (an older client's shape)"
        );
    }

    #[test]
    fn submit_user_input_carries_an_audio_envelope_when_present() {
        // Voice v1 (rubric 8): a follow-up may normalize its input as a full
        // InputEnvelope. Here the common voice shape — one Audio block
        // referencing a stored artifact, no transcript yet (the daemon
        // produces it) — round-trips exactly, and an older payload without
        // the key still parses to None (covered by the test above).
        use crate::artifact::{ArtifactRef, DataClassification};
        use crate::input::{AudioArtifact, InputBlock, InputSource, ScopeLevel};

        let audio = AudioArtifact {
            original: ArtifactRef {
                id: crate::ids::ArtifactId::new(),
                media_type: "audio/wav".to_string(),
                byte_length: 64_000,
                sha256: "b".repeat(64),
                sensitivity: DataClassification::Confidential,
            },
            transcript: None,
            duration_ms: Some(2_000),
            sample_rate_hz: Some(16_000),
        };
        let body = CommandBody::SubmitUserInput {
            session_id: SessionId::new(),
            text: String::new(),
            mode: AgentMode::Build,
            model: None,
            envelope: Some(InputEnvelope {
                source: crate::input::InputSource::Voice,
                blocks: vec![InputBlock::Audio(audio)],
                scope: ScopeLevel::Session,
                attachments: vec![],
            }),
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(json.contains("envelope"), "envelope on the wire: {json}");
        assert_eq!(
            serde_json::from_str::<CommandBody>(&json).expect("deserialize"),
            body
        );
        // The source survives verbatim (voice capture is attributed as such).
        let CommandBody::SubmitUserInput {
            envelope: Some(envelope),
            ..
        } = serde_json::from_str::<CommandBody>(&json).expect("deserialize")
        else {
            panic!("expected SubmitUserInput with an envelope");
        };
        assert_eq!(envelope.source, InputSource::Voice);
    }

    #[test]
    fn put_artifact_round_trips_and_classification_is_explicit() {
        // Voice v1 (rubric 8): the upload command carries its classification
        // explicitly — nothing downstream may guess it from the bytes.
        let body = CommandBody::PutArtifact {
            media_type: "audio/wav".to_string(),
            bytes_base64: "UklGRg==".to_string(),
            sensitivity: DataClassification::Confidential,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(json.contains("audio/wav"));
        assert!(json.contains("Confidential"));
        assert_eq!(
            serde_json::from_str::<CommandBody>(&json).expect("deserialize"),
            body
        );
    }

    #[test]
    fn every_command_body_round_trips() {
        round_trip(CommandBody::CreateSession {
            workspace: WorkspaceId::new(),
            title: "fix the failing test".to_string(),
            repository: Some("/home/user/project".to_string()),
            internal: false,
            parent_session_id: None,
            parent_run_id: None,
        });
        round_trip(CommandBody::CloseSession {
            session_id: SessionId::new(),
        });
        round_trip(CommandBody::AttachSession {
            session_id: SessionId::new(),
            last_seen_sequence: Some(42),
            subscriptions: vec![Subscription::SessionSummary],
            requested_role: ClientRole::Contributor,
            repository: Some("/home/user/project".to_string()),
        });
        round_trip(CommandBody::SubmitUserInput {
            session_id: SessionId::new(),
            text: "try again".to_string(),
            mode: AgentMode::Build,
            model: Some(ModelId("claude-sonnet-5".to_string())),
            envelope: None,
        });
        // Also round-trip the unpinned continuation (no mid-conversation switch):
        // the field is absent on the wire and reparses to None.
        round_trip(CommandBody::SubmitUserInput {
            session_id: SessionId::new(),
            text: "try again".to_string(),
            mode: AgentMode::Build,
            model: None,
            envelope: None,
        });
        // Voice v1 (rubric 8): the upload half of the multimodal input path.
        round_trip(CommandBody::PutArtifact {
            media_type: "audio/wav".to_string(),
            bytes_base64: "UklGRg==".to_string(),
            sensitivity: DataClassification::Confidential,
        });
        round_trip(CommandBody::StartRun {
            session_id: SessionId::new(),
            objective: "diagnose the failing test".to_string(),
            mode: AgentMode::Build,
            repository: Some("/home/user/project".to_string()),
            model: Some(ModelId("claude-sonnet-5".to_string())),
        });
        round_trip(CommandBody::ResolveApproval {
            approval_id: ApprovalId::new(),
            decision: ApprovalDecision::Approve,
            scope: ApprovalScope::Run,
        });
        round_trip(CommandBody::ResolveQuestion {
            question_id: QuestionId::new(),
            outcome: QuestionOutcome::Answered {
                answers: vec![vec!["SQLite (Recommended)".to_string()]],
            },
        });
        round_trip(CommandBody::CancelRun {
            run_id: RunId::new(),
        });
        round_trip(CommandBody::PauseRun {
            run_id: RunId::new(),
        });
        round_trip(CommandBody::ResumeRun {
            run_id: RunId::new(),
        });
        round_trip(CommandBody::QueueSteering {
            run_id: RunId::new(),
            text: "focus on the parser".to_string(),
        });
        round_trip(CommandBody::UpdateIdeContext {
            session_id: SessionId::new(),
            update: IdeContextUpdate {
                active_file: Some("src/lib.rs".to_string()),
                dirty_buffers: vec![crate::ide::DirtyBufferDigest {
                    path: "src/lib.rs".to_string(),
                    sha256: "deadbeef".to_string(),
                    byte_length: 12,
                }],
                ..Default::default()
            },
        });
        round_trip(CommandBody::CreateDocument {
            title: "Payments Runbook".to_string(),
            scope: Some("repository".to_string()),
            repository: Some("/home/user/project".to_string()),
            initial_markdown: Some("# Payments Runbook\n\nBody.\n".to_string()),
        });
        // The minimal create (an older client's shape): only the title.
        round_trip(CommandBody::CreateDocument {
            title: "Notes".to_string(),
            scope: None,
            repository: None,
            initial_markdown: None,
        });
        round_trip(CommandBody::CheckDocuments {
            repository: Some("/home/user/project".to_string()),
            session_id: Some(SessionId::new()),
        });
        round_trip(CommandBody::CheckDocuments {
            repository: None,
            session_id: None,
        });
        round_trip(CommandBody::MutateDocument {
            document_id: DocumentId::new(),
            mutation: DocumentMutation::EditText {
                block_id: "b1".to_string(),
                position: 0,
                delete_len: 0,
                insert: "hello".to_string(),
            },
        });
        round_trip(CommandBody::AcquireDocumentLease {
            lease: DocumentEditLease {
                document_id: DocumentId::new(),
                block_id: Some("b1".to_string()),
            },
            ttl_seconds: Some(300),
        });
        round_trip(CommandBody::ReleaseDocumentLease {
            lease_id: "lease-1".to_string(),
        });
        round_trip(CommandBody::PublishDocument {
            document_id: DocumentId::new(),
            target: crate::document::PublishTarget::RepositoryFile {
                path: "docs/architecture.md".to_string(),
            },
        });
        round_trip(CommandBody::StartWorkflow {
            expected_definition_digest: None,
            manifest: "schema_version: 1\nid: wf\nversion: 1\nsteps: []\n".to_string(),
            workflow_id: None,
            inputs: serde_json::json!({ "pull_request": 42 }),
            repository: Some("/home/user/project".to_string()),
        });
        // A named-workflow start (the `/fix-ci` shape): no inline manifest, a
        // resolved workflow id instead.
        round_trip(CommandBody::StartWorkflow {
            expected_definition_digest: None,
            manifest: String::new(),
            workflow_id: Some("repair-github-check".to_string()),
            inputs: serde_json::json!({ "pull_request": 42 }),
            repository: Some("/home/user/project".to_string()),
        });
        round_trip(CommandBody::PauseWorkflow {
            workflow_run_id: "wfrun-abc123".to_string(),
        });
        round_trip(CommandBody::ResumeWorkflow {
            workflow_run_id: "wfrun-abc123".to_string(),
        });
        round_trip(CommandBody::RetryWorkflowNode {
            workflow_run_id: "wfrun-abc123".to_string(),
            node_id: "verify".to_string(),
        });
        round_trip(CommandBody::CancelWorkflow {
            workflow_run_id: "wfrun-abc123".to_string(),
        });
        round_trip(CommandBody::ReadWorkflowRun {
            workflow_run_id: "wfrun-abc123".to_string(),
        });
        round_trip(CommandBody::ProposePromotion {
            kind: "router".to_string(),
            name: "tool-selection".to_string(),
            version: 12,
            requires_permission_review: false,
        });
        round_trip(CommandBody::AdvancePromotion {
            candidate_id: "cand-abc123".to_string(),
            action: PromotionAction::RunRegression,
        });
        round_trip(CommandBody::AdvancePromotion {
            candidate_id: "cand-abc123".to_string(),
            action: PromotionAction::ReviewPermissions,
        });
        round_trip(CommandBody::AdvancePromotion {
            candidate_id: "cand-abc123".to_string(),
            action: PromotionAction::StartShadow,
        });
        round_trip(CommandBody::AdvancePromotion {
            candidate_id: "cand-abc123".to_string(),
            action: PromotionAction::StartCanary,
        });
        round_trip(CommandBody::AdvancePromotion {
            candidate_id: "cand-abc123".to_string(),
            action: PromotionAction::ObserveCanary,
        });
        round_trip(CommandBody::AdvancePromotion {
            candidate_id: "cand-abc123".to_string(),
            action: PromotionAction::FinishCanary,
        });
        round_trip(CommandBody::ApprovePromotion {
            candidate_id: "cand-abc123".to_string(),
        });
        round_trip(CommandBody::RollbackPromotion {
            candidate_id: "cand-abc123".to_string(),
        });
        round_trip(CommandBody::ReadBlackboard {
            workflow_run_id: "wfrun-abc123".to_string(),
            kind: Some("finding".to_string()),
            include_superseded: true,
            board_repository: None,
        });
        // The repository-board read (Phase B kanban): board_repository set, the
        // run id left empty for the daemon to resolve.
        round_trip(CommandBody::ReadBlackboard {
            workflow_run_id: String::new(),
            kind: Some("task".to_string()),
            include_superseded: false,
            board_repository: Some("/home/user/project".to_string()),
        });
        round_trip(CommandBody::PostBlackboardItem {
            scope: crate::blackboard::BlackboardScope::RepositoryBoard {
                repository: "/home/user/project".to_string(),
            },
            item: crate::blackboard::BlackboardItemDraft {
                kind: "task".to_string(),
                payload: serde_json::json!({ "title": "wire the DAG viewer" }),
                confidence: None,
                evidence: Vec::new(),
                status: Some("todo".to_string()),
                assignee: Some("dana".to_string()),
                ordinal: Some(1),
            },
        });
        round_trip(CommandBody::UpdateBlackboardItem {
            scope: crate::blackboard::BlackboardScope::RepositoryBoard {
                repository: "/home/user/project".to_string(),
            },
            item_id: "0192-item".to_string(),
            status: Some("doing".to_string()),
            assignee: None,
            ordinal: Some(2),
            payload: None,
        });
        round_trip(CommandBody::ReadSessionEvents {
            session_id: SessionId::new(),
            after_sequence: 500,
            limit: 200,
        });
        round_trip(CommandBody::RunUserShell {
            session_id: SessionId::new(),
            command: "cargo test -q".to_string(),
        });
        round_trip(CommandBody::RememberMemory {
            session_id: SessionId::new(),
            text: "prefer ripgrep over grep in this repo".to_string(),
        });
    }

    #[test]
    fn read_session_events_omits_zero_defaults_and_reparses() {
        // The from-the-start read sends neither optional key, and such a payload
        // (also what a minimal encoder emits) reparses with both zeroed — the
        // server then applies its own default page size.
        let body = CommandBody::ReadSessionEvents {
            session_id: SessionId::new(),
            after_sequence: 0,
            limit: 0,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(
            !json.contains("after_sequence"),
            "zero after_sequence skipped: {json}"
        );
        assert!(!json.contains("limit"), "zero limit skipped: {json}");
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body);
    }

    #[test]
    fn propose_promotion_without_the_review_flag_reparses_to_false() {
        // A payload missing `requires_permission_review` entirely (what an
        // older client, or one hand-constructing the minimal shape, sends)
        // must still parse — defaulted to `false` — rather than erroring.
        let json = serde_json::json!({
            "type": "ProposePromotion",
            "kind": "skill",
            "name": "rust-ci",
            "version": 1,
        });
        let parsed: CommandBody = serde_json::from_value(json).expect("deserialize");
        assert_eq!(
            parsed,
            CommandBody::ProposePromotion {
                kind: "skill".to_string(),
                name: "rust-ci".to_string(),
                version: 1,
                requires_permission_review: false,
            }
        );
    }

    #[test]
    fn unknown_promotion_action_tag_deserializes_to_unknown() {
        // Forward-compatibility (RULE 1) for the nested PromotionAction enum,
        // exactly like CommandBody's own Unknown fallback.
        let parsed: PromotionAction = serde_json::from_value(
            serde_json::json!({ "type": "RunOnnxInference", "confidence": 0.9 }),
        )
        .expect("unknown tag must parse, not error");
        assert!(matches!(parsed, PromotionAction::Unknown));
    }

    #[test]
    fn read_blackboard_omits_default_filter_and_flag() {
        // A live-only, all-kinds read sends neither optional key, and such a payload
        // (also what an older client emits) reparses with both defaulted.
        let body = CommandBody::ReadBlackboard {
            workflow_run_id: "wfrun-abc123".to_string(),
            kind: None,
            include_superseded: false,
            board_repository: None,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(!json.contains("kind"), "absent kind is skipped: {json}");
        assert!(
            !json.contains("include_superseded"),
            "default (false) include_superseded is skipped: {json}"
        );
        assert!(
            !json.contains("board_repository"),
            "absent board_repository is skipped (an older client's shape): {json}"
        );
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body);
    }

    #[test]
    fn start_workflow_omits_null_inputs_and_reparses() {
        // A workflow with no inputs sends no `inputs` key, and such a payload
        // (also what an older client emits) reparses with `inputs` defaulted to
        // null.
        let body = CommandBody::StartWorkflow {
            expected_definition_digest: None,
            manifest: "schema_version: 1\nid: wf\nversion: 1\nsteps: []\n".to_string(),
            workflow_id: None,
            inputs: serde_json::Value::Null,
            repository: None,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(!json.contains("inputs"), "null inputs are skipped: {json}");
        assert!(
            !json.contains("repository"),
            "an absent repository is skipped on the wire: {json}"
        );
        assert!(
            !json.contains("workflow_id"),
            "an absent workflow_id is skipped on the wire (an older client's shape): {json}"
        );
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body, "a payload without either key defaults them");
    }

    #[test]
    fn start_workflow_carries_a_repository_when_present() {
        // A workflow run bound to a repository (Phase 5 T5) serializes the key,
        // and round-trips back to the same value — the durable store persists it
        // so recovery drives the run's agent nodes in the right checkout.
        let body = CommandBody::StartWorkflow {
            expected_definition_digest: None,
            manifest: "schema_version: 1\nid: wf\nversion: 1\nsteps: []\n".to_string(),
            workflow_id: None,
            inputs: serde_json::Value::Null,
            repository: Some("/home/user/project".to_string()),
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(
            json.contains("/home/user/project"),
            "repository on the wire: {json}"
        );
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body);
    }

    #[test]
    fn create_document_omits_absent_optionals() {
        // A title-only create sends none of the optional keys, and such a
        // payload (also what an older client emits) reparses with all three
        // defaulted to None.
        let body = CommandBody::CreateDocument {
            title: "Notes".to_string(),
            scope: None,
            repository: None,
            initial_markdown: None,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(!json.contains("scope"), "absent scope is skipped: {json}");
        assert!(
            !json.contains("repository"),
            "absent repository is skipped: {json}"
        );
        assert!(
            !json.contains("initial_markdown"),
            "absent markdown is skipped: {json}"
        );
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body);
    }

    #[test]
    fn acquire_document_lease_omits_absent_ttl_and_block() {
        // A whole-document lease with the default TTL sends neither optional key,
        // and such a payload (also what an older client would emit) reparses with
        // both defaulted.
        let body = CommandBody::AcquireDocumentLease {
            lease: DocumentEditLease {
                document_id: DocumentId::new(),
                block_id: None,
            },
            ttl_seconds: None,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(!json.contains("ttl_seconds"));
        assert!(!json.contains("block_id"));
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body);
    }

    #[test]
    fn attach_session_omits_absent_sequence() {
        let command = Command {
            command_id: CommandId::new(),
            idempotency_key: "idem-2".to_string(),
            expected_revision: None,
            body: CommandBody::AttachSession {
                session_id: SessionId::new(),
                last_seen_sequence: None,
                subscriptions: vec![],
                requested_role: ClientRole::Observer,
                repository: None,
            },
        };
        let json = serde_json::to_string(&command).expect("serialize");
        assert!(!json.contains("last_seen_sequence"));
        assert!(!json.contains("expected_revision"));
        let parsed: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(command, parsed);
    }

    #[test]
    fn unknown_command_tag_deserializes_to_unknown() {
        let parsed: CommandBody = serde_json::from_value(
            serde_json::json!({ "type": "TeleportRepository", "coords": [1, 2] }),
        )
        .expect("unknown tag must parse, not error");
        assert!(matches!(parsed, CommandBody::Unknown));
    }

    #[test]
    fn restore_checkpoint_round_trip() {
        let run_id = RunId::new();
        let checkpoint = CheckpointId::new();
        let body = CommandBody::RestoreCheckpoint { run_id, checkpoint };
        let json = serde_json::to_string(&body).expect("serialize");
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body);
        assert_eq!(body.named_resources(), vec![NamedResource::Run(run_id)]);
    }

    #[test]
    fn fork_session_round_trip() {
        let session_id = SessionId::new();
        let checkpoint = CheckpointId::new();
        let body_no_name = CommandBody::ForkSession {
            session_id,
            checkpoint,
            name: None,
        };
        let json = serde_json::to_string(&body_no_name).expect("serialize");
        assert!(!json.contains("name"));
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body_no_name);
        assert_eq!(
            body_no_name.named_resources(),
            vec![NamedResource::Session(session_id)]
        );

        let body_with_name = CommandBody::ForkSession {
            session_id,
            checkpoint,
            name: Some("my fork".to_string()),
        };
        let json = serde_json::to_string(&body_with_name).expect("serialize");
        assert!(json.contains("my fork"));
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body_with_name);
    }

    #[test]
    fn prompt_queue_commands_round_trip() {
        let session_id = SessionId::new();
        let prompt_id = PromptId::new();

        let queue_cmd = CommandBody::QueuePrompt {
            session_id,
            text: "also add tests".to_string(),
            mode: AgentMode::Build,
            delivery: PromptDelivery::Queue,
        };
        let json = serde_json::to_string(&queue_cmd).expect("serialize");
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, queue_cmd);
        assert_eq!(
            queue_cmd.named_resources(),
            vec![NamedResource::Session(session_id)]
        );

        let update_cmd = CommandBody::UpdateQueuedPrompt {
            session_id,
            prompt_id,
            text: Some("updated prompt".to_string()),
            delivery: Some(PromptDelivery::Steer),
        };
        let json = serde_json::to_string(&update_cmd).expect("serialize");
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, update_cmd);

        let update_cmd_none = CommandBody::UpdateQueuedPrompt {
            session_id,
            prompt_id,
            text: None,
            delivery: None,
        };
        let json_none = serde_json::to_string(&update_cmd_none).expect("serialize");
        assert!(!json_none.contains("text"));
        assert!(!json_none.contains("delivery"));
        let parsed_none: CommandBody = serde_json::from_str(&json_none).expect("deserialize");
        assert_eq!(parsed_none, update_cmd_none);

        let promote_cmd = CommandBody::PromoteQueuedPrompt {
            session_id,
            prompt_id,
        };
        let json = serde_json::to_string(&promote_cmd).expect("serialize");
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, promote_cmd);

        let delete_cmd = CommandBody::DeleteQueuedPrompt {
            session_id,
            prompt_id,
        };
        let json = serde_json::to_string(&delete_cmd).expect("serialize");
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, delete_cmd);
    }

    #[test]
    fn read_artifact_round_trips() {
        let body = CommandBody::ReadArtifact {
            artifact_id: ArtifactId::new(),
            offset: 7,
            limit: 1024,
            expected_sha256: "ab".repeat(32),
        };
        let json = serde_json::to_string(&body).expect("serialize");
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body);
    }

    /// Pins the ownership gate for budgets. The integration test in
    /// `crates/daemon/tests/analytics_it.rs` calls the analytics handlers
    /// directly, so it stays green even if this returns `Vec::new()` — and an
    /// empty `Vec` makes the ownership gate pass VACUOUSLY, which is how
    /// `ListSessions` once leaked across users. This test is the one that fails.
    #[test]
    fn manage_analytics_budget_named_resources() {
        let id = "budget-7".to_string();
        for request in [
            AnalyticsBudgetRequest::Get { id: id.clone() },
            AnalyticsBudgetRequest::Delete { id: id.clone() },
        ] {
            let cmd = CommandBody::ManageAnalyticsBudget { request };
            assert_eq!(
                cmd.named_resources(),
                vec![NamedResource::Budget(id.as_str())],
                "Get/Delete must name the budget they act on"
            );
        }

        // Create and List name no existing resource, so an empty Vec is correct
        // here and is NOT the vacuous-gate bug.
        let list = CommandBody::ManageAnalyticsBudget {
            request: AnalyticsBudgetRequest::List {
                query: Default::default(),
            },
        };
        assert!(list.named_resources().is_empty());
    }

    #[test]
    fn workflow_catalog_names_the_private_daemon_store() {
        let command = CommandBody::ListWorkflows {
            repository: Some("/repo".to_owned()),
        };
        assert_eq!(
            command.named_resources(),
            vec![NamedResource::DaemonStore(DaemonStore::Workflows)]
        );
        let encoded = serde_json::to_string(&command).unwrap();
        assert_eq!(
            serde_json::from_str::<CommandBody>(&encoded).unwrap(),
            command
        );
    }

    #[test]
    fn manage_automation_binding_named_resources() {
        let id = AutomationBindingId::new();
        let get_cmd = CommandBody::ManageAutomationBinding {
            request: AutomationBindingRequest::Get { id },
        };
        assert_eq!(
            get_cmd.named_resources(),
            vec![NamedResource::AutomationBinding(id)]
        );

        let update_cmd = CommandBody::ManageAutomationBinding {
            request: AutomationBindingRequest::Update {
                id,
                patch: Default::default(),
            },
        };
        assert_eq!(
            update_cmd.named_resources(),
            vec![NamedResource::AutomationBinding(id)]
        );

        let delete_cmd = CommandBody::ManageAutomationBinding {
            request: AutomationBindingRequest::Delete { id },
        };
        assert_eq!(
            delete_cmd.named_resources(),
            vec![NamedResource::AutomationBinding(id)]
        );

        let list_cmd = CommandBody::ManageAutomationBinding {
            request: AutomationBindingRequest::List {
                query: Default::default(),
            },
        };
        assert_eq!(list_cmd.named_resources(), Vec::<NamedResource>::new());
    }
}
