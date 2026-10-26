/**
 * Generated from the authoritative Rust protocol schema.
 * Do not edit by hand; run `npm run generate`.
 */

export type Payload =
  | {
      command_id: string;
      page: KnowledgePage;
      type: "KnowledgePage";
    }
  | {
      type: "Ping";
    }
  | {
      type: "Pong";
    }
  | {
      type: "DaemonStatusRequest";
    }
  | {
      /**
       * Count of runs in a non-terminal state (`Queued`, `Preparing`, `Running`, `WaitingForApproval`, `WaitingForUserInput`, `Paused`, `Recovering` — every [`RunState`](crate::RunState) except `Completed`, `Failed`, and `Cancelled`). Used to gate an automatic daemon restart: only an idle daemon (`active_run_count == 0`) is safe to restart without losing in-flight work. Defaulted for wire compatibility with daemons predating this field.
       */
      active_run_count?: number;
      boot_count: number;
      /**
       * The running daemon's per-build id ([`codypendent_protocol::BUILD_ID`](crate::BUILD_ID)). Defaulted for wire compatibility with daemons predating this field (mirrors [`ServerHello::build_id`](crate::ServerHello::build_id)).
       */
      build_id?: string;
      daemon_version: string;
      database_path: string;
      instance_id: string;
      /**
       * Sanitized, de-duplicated optional-integration failures observed by the running daemon. Defaulted so older daemon/client pairs remain wire compatible. Secrets and raw extension output must never enter this list.
       */
      integration_issues?: string[];
      pid: number;
      protocol_version: ProtocolVersion;
      session_count: number;
      socket_path: string;
      started_at: string;
      type: "DaemonStatusResponse";
      uptime_seconds: number;
    }
  | {
      type: "Shutdown";
    }
  | {
      type: "ShutdownAck";
    }
  | {
      type: "ShutdownIfIdle";
    }
  | {
      active_run_count?: number;
      type: "ShutdownRefused";
    }
  | {
      code: string;
      message: string;
      retryable: boolean;
      type: "Error";
    }
  | {
      capabilities: ClientCapabilities;
      client_name: string;
      client_version: string;
      /**
       * Present when resuming a prior connection.
       */
      resume_token?: string | null;
      /**
       * Protocol versions the client can speak, best first.
       */
      supported_protocols: ProtocolVersion[];
      type: "ClientHello";
    }
  | {
      /**
       * The running daemon's per-build id ([`codypendent_protocol::BUILD_ID`](crate::BUILD_ID)), used by the client to detect that the connected daemon is a different build than the client binary (a stale in-memory daemon after a reinstall). Defaulted for wire compatibility with daemons predating this field — an empty string is treated by the client as "unknown build" (i.e. a mismatch).
       */
      build_id?: string;
      daemon_instance: string;
      daemon_version: string;
      /**
       * How often the client should expect (and send) heartbeats.
       */
      heartbeat_interval_ms: number;
      /**
       * A fresh [`ResumeToken`] for this connection's identity. The client stores it opaquely and presents it on its next `ClientHello`, so a reconnect resumes the same client identity even across a client-process restart. Optional and defaulted for wire compatibility with older daemons/clients.
       */
      resume_token?: string | null;
      selected_protocol: ProtocolVersion;
      type: "ServerHello";
    }
  | {
      body: CommandBody;
      command_id: string;
      /**
       * Optimistic-concurrency guard: apply only if the session is still at this revision.
       */
      expected_revision?: number | null;
      /**
       * Client-chosen key; the same key must never apply twice.
       */
      idempotency_key: string;
      type: "Command";
    }
  | {
      command_id: string;
      /**
       * The run a `StartRun` created, so the issuing client can bind to exactly that run (never a concurrent client's run that happened to start first). Absent on every other command; defaulted for wire compatibility with older daemons.
       */
      created_run?: string | null;
      sequence?: number | null;
      type: "CommandAccepted";
    }
  | {
      /**
       * Stable machine-readable code. Never parse `message` to decide behaviour.
       */
      code: string;
      correlation_id: string;
      details?: JsonValue;
      /**
       * Human-readable explanation.
       */
      message: string;
      /**
       * Whether an identical retry could succeed.
       */
      retryable: boolean;
      type: "CommandRejected";
      /**
       * A suggested next step the client can surface as an affordance.
       */
      user_action?: UserAction | null;
    }
  | {
      command_id: string;
      grant: DocumentLeaseGrant;
      type: "DocumentLeaseGranted";
    }
  | {
      command_id: string;
      document_id: string;
      type: "DocumentCreated";
    }
  | {
      command_id: string;
      session_id: string;
      type: "SessionForked";
    }
  | {
      command_id: string;
      sessions: SessionSummary[];
      type: "SessionList";
    }
  | {
      command_id: string;
      page: SessionSearchPage;
      type: "SessionSearchResults";
    }
  | {
      command_id: string;
      page: SessionHistoryPage;
      session_id: string;
      type: "SessionHistory";
    }
  | {
      command_id: string;
      session: SessionSummary;
      type: "SessionLifecycleApplied";
    }
  | {
      command_id: string;
      session_id: string;
      tombstoned?: boolean;
      type: "SessionDeleted";
    }
  | {
      artifact: ArtifactRef;
      command_id: string;
      type: "SessionExported";
    }
  | {
      command_id: string;
      run_id: string;
      type: "EditorActionAccepted";
    }
  | {
      command_id: string;
      page: InboxPage;
      type: "InboxPage";
    }
  | {
      command_id: string;
      entry: InboxEntry;
      type: "InboxEntryApplied";
    }
  | {
      command_id: string;
      page: AnalyticsPage;
      type: "AnalyticsResults";
    }
  | {
      command_id: string;
      result: AnalyticsExportResult;
      type: "AnalyticsExported";
    }
  | {
      binding: AutomationBinding;
      command_id: string;
      type: "AutomationBindingResult";
    }
  | {
      command_id: string;
      page: AutomationBindingPage;
      type: "AutomationBindingPage";
    }
  | {
      binding_id: string;
      command_id: string;
      type: "AutomationBindingDeleted";
    }
  | {
      budget: AnalyticsBudget;
      command_id: string;
      type: "AnalyticsBudgetResult";
    }
  | {
      command_id: string;
      page: AnalyticsBudgetPage;
      type: "AnalyticsBudgetPage";
    }
  | {
      budget_id: string;
      command_id: string;
      type: "AnalyticsBudgetDeleted";
    }
  | {
      command_id: string;
      receipt: BundleExportReceipt;
      type: "BundleExported";
    }
  | {
      command_id: string;
      receipt: BundleImportReceipt;
      type: "BundleImported";
    }
  | {
      command_id: string;
      matches: FileMatchWire[];
      query: string;
      truncated?: boolean;
      type: "FileSearchResults";
    }
  | {
      command_id: string;
      /**
       * Documents the sweep examined.
       */
      documents_checked: number;
      /**
       * Symbol links resolved (and persisted) against the code graph.
       */
      links_resolved: number;
      /**
       * Staleness findings (signature changed / symbol disappeared).
       */
      stale_findings: number;
      /**
       * Maintain-mode suggestions filed from those findings (never direct edits; each still needs a human accept).
       */
      suggestions_filed: number;
      type: "DocsCheckCompleted";
    }
  | {
      command_id: string;
      type: "WorkflowRunStarted";
      workflow_run_id: string;
    }
  | {
      approval_id: string;
      changed_files: string[];
      command_id: string;
      git_action: string;
      target: string;
      type: "DocumentPublishRequested";
    }
  | {
      candidate_id: string;
      command_id: string;
      type: "PromotionProposed";
    }
  | {
      command_id: string;
      plugins: UiPluginLifecycleStatus[];
      type: "UiPluginLifecycle";
    }
  | {
      artifact: ArtifactRef;
      command_id: string;
      type: "ArtifactStored";
    }
  | {
      artifact_id: string;
      bytes_base64: string;
      eof: boolean;
      offset: number;
      sha256: string;
      type: "ArtifactChunk";
    }
  | {
      command_id: string;
      memory: MemoryView;
      type: "Memory";
    }
  | {
      command_id: string;
      forgotten: string[];
      type: "MemoryForgotten";
    }
  | {
      command_id: string;
      evidence: MemoryEvidence;
      type: "MemoryEvidence";
    }
  | {
      command_id: string;
      items: BlackboardItemView[];
      type: "BlackboardItems";
    }
  | {
      /**
       * Who the card is assigned to, if anyone.
       */
      assignee?: string | null;
      author: JsonValue;
      /**
       * The repository this item's board serves, when the item lives on a repository task board rather than a real workflow run (its `workflow_run_id` is then the synthetic [`board_scope_id`]). Additive: an older daemon sends none and every field below parses back defaulted.
       */
      board_scope?: string | null;
      /**
       * The author's self-reported confidence in `[0, 1]`, if given.
       */
      confidence?: number | null;
      /**
       * Evidence references grounding the artifact (opaque JSON). Claim-like kinds require at least one; the store enforces it.
       */
      evidence?: JsonValue[];
      /**
       * The artifact's stable id (a UUIDv7 string).
       */
      id: string;
      /**
       * The typed artifact kind (`finding`, `decision`, `hypothesis`, …), as the manifest-facing string the `BlackboardStore` records.
       */
      kind: string;
      /**
       * The card's position within its column (lower sorts first).
       */
      ordinal?: number | null;
      payload: JsonValue;
      /**
       * The artifact's revision within its supersession chain (1 for an original).
       */
      revision: number;
      /**
       * The board column (`todo` / `doing` / `review` / `done`, or a validated free string), when this item is a board card.
       */
      status?: string | null;
      /**
       * The id of the item that superseded this one, if any — a live item has `None`.
       */
      superseded_by?: string | null;
      type: "BlackboardPosted";
      /**
       * The workflow run whose board holds it.
       */
      workflow_run_id: string;
    }
  | {
      command_id: string;
      item: BlackboardItemView;
      type: "BlackboardItemApplied";
    }
  | {
      command_id: string;
      events: SessionEvent[];
      has_more: boolean;
      session_id: string;
      through: number;
      type: "SessionEventsPage";
    }
  | {
      command_id: string;
      snapshot: WorkflowRunSnapshot;
      type: "WorkflowRunSnapshot";
    }
  | {
      event: WorkflowEvent;
      type: "WorkflowEvent";
    }
  | {
      actor: Actor;
      body: EventBody;
      causation_id?: string | null;
      correlation_id?: string | null;
      occurred_at: string;
      sequence: number;
      type: "Event";
    }
  | {
      document_id: string;
      /**
       * The document revision this update advances to (for ordering/UX).
       */
      revision: number;
      type: "DocumentSync";
      /**
       * Opaque CRDT bytes. Serialized on the wire as a JSON array of byte values (see the [`byte_vec`] module) — the framing layer emits plain `serde_json`, so there is no base64 step; a client sends the raw bytes and they round-trip as numbers. Every sync currently carries a **full CRDT snapshot** (`ExportMode::Snapshot`), not an incremental delta — the client replica relies on this (an empty or lagged replica converges on the next sync, and re-importing a snapshot is an idempotent no-op). If a future change sends deltas to stay under the frame-size bound, the client replica (`knowledge::docs::DocumentReplica`) must be updated in lockstep, or a replica missing a delta's causal dependencies would silently fail to converge.
       */
      update: Byte[];
    }
  | {
      command_id: string;
      report: CodeGraphScanReport;
      type: "CodeGraphBuilt";
    }
  | {
      command_id: string;
      models: ModelProbe[];
      type: "ModelProbes";
    }
  | {
      command_id: string;
      status: CodeGraphStatusView;
      type: "CodeGraphStatus";
    }
  | {
      command_id: string;
      page: CodeGraphPage;
      type: "CodeGraphPage";
    }
  | {
      catchup: Catchup;
      type: "Catchup";
    }
  | {
      message: UiWireMessage;
      type: "RemoteUi";
    }
  | {
      command_id: string;
      packages: MarketplacePackageView[];
      type: "MarketplaceSearchResults";
    }
  | {
      command_id: string;
      package: MarketplacePackageView;
      type: "MarketplaceLifecycle";
    }
  | {
      command_id: string;
      reference: SecretReferenceView;
      type: "SecretReferenceApplied";
    }
  | {
      command_id: string;
      expires_at: string;
      lease_id: string;
      type: "SecretLeaseBound";
    }
  | {
      command_id: string;
      references: SecretReferenceView[];
      type: "SecretReferenceList";
    }
  | {
      command_id: string;
      reference_id: string;
      type: "SecretReferenceRevoked";
    }
  | {
      command_id: string;
      identity: FederatedRepositoryIdentityView;
      type: "FederatedIdentityEstablished";
    }
  | {
      command_id: string;
      policy: GraphPublicationPolicyView;
      type: "PublicationPolicy";
    }
  | {
      command_id: string;
      summary: PublicationBatchSummary;
      type: "GraphFactsPublished";
    }
  | {
      command_id: string;
      tombstone_id: string;
      type: "GraphTombstoned";
    }
  | {
      command_id: string;
      page: FederatedGraphPage;
      type: "FederatedGraphResult";
    }
  | {
      command_id: string;
      report: BlastRadiusReport;
      type: "BlastRadiusResult";
    }
  | {
      command_id: string;
      report: MigrationPlanReport;
      type: "MigrationPlanResult";
    }
  | {
      command_id: string;
      suggestions: ReviewerSuggestions;
      type: "ReviewerSuggestionsResult";
    }
  | {
      campaign_id: string;
      command_id: string;
      type: "CampaignCreated";
    }
  | {
      command_id: string;
      detail: CampaignDetailView;
      type: "CampaignDetail";
    }
  | {
      campaigns: CampaignView[];
      command_id: string;
      type: "CampaignList";
    }
  | {
      campaign: CampaignView;
      command_id: string;
      type: "CampaignExecuted";
    }
  | {
      campaign_id: string;
      command_id: string;
      type: "CampaignCancelled";
    }
  | {
      type: "Unknown";
    };
type KnowledgeSection =
  | {
      type: "Skills";
    }
  | {
      type: "Memories";
    }
  | {
      type: "Learnings";
    }
  | {
      type: "Documents";
    }
  | {
      type: "Unknown";
    };
/**
 * The specific change a command requests. A wire enum: internally tagged with an [`CommandBody::Unknown`] fallback so a command from a newer client deserializes and is rejected structurally rather than crashing the peer.
 */
type CommandBody =
  | {
      limit?: number;
      offset?: number;
      section: KnowledgeSection;
      session_id: string;
      type: "ReadKnowledge";
    }
  | {
      allow_unsigned?: boolean;
      artifact_base64: string;
      manifest_toml: string;
      type: "InstallUiPlugin";
    }
  | {
      plugin_id: string;
      type: "SmokeTestUiPlugin";
    }
  | {
      plugin_id: string;
      scope: string;
      session_id?: string | null;
      type: "EnableUiPlugin";
    }
  | {
      type: "ListUiPlugins";
    }
  | {
      allow_unsigned?: boolean;
      artifact_base64: string;
      manifest_toml: string;
      plugin_id: string;
      type: "UpdateUiPlugin";
    }
  | {
      approval_receipt: string;
      plugin_id: string;
      type: "ApproveUiPluginUpdate";
    }
  | {
      approval_receipt: string;
      plugin_id: string;
      type: "RejectUiPluginUpdate";
    }
  | {
      plugin_id: string;
      type: "RevokeUiPlugin";
    }
  | {
      publisher_id: string;
      type: "RemoveTrustedUiPublisher";
    }
  | {
      /**
       * Hard cap on returned rows (the daemon also caps at 200).
       */
      limit?: number | null;
      type: "ListSessions";
      /**
       * Restrict to one workspace; `None` lists all.
       */
      workspace?: string | null;
    }
  | {
      query: SessionSearchQuery;
      type: "SearchSessions";
    }
  | {
      cursor?: string | null;
      limit?: number;
      session_id: string;
      type: "ReadSessionHistory";
    }
  | {
      action: SessionLifecycleAction;
      session_id: string;
      type: "MutateSessionLifecycle";
    }
  | {
      action: EditorNativeAction;
      context: EditorActionContext;
      model?: string | null;
      session_id: string;
      type: "RunEditorAction";
    }
  | {
      query?: InboxListQuery;
      type: "ListInbox";
    }
  | {
      mutation: InboxMutation;
      type: "MutateInbox";
    }
  | {
      query?: AnalyticsQuery;
      type: "QueryAnalytics";
    }
  | {
      request: AnalyticsExportRequest;
      type: "ExportAnalytics";
    }
  | {
      request: AutomationBindingRequest;
      type: "ManageAutomationBinding";
    }
  | {
      request: AnalyticsBudgetRequest;
      type: "ManageAnalyticsBudget";
    }
  | {
      request: BundleExportRequest;
      type: "ExportBundle";
    }
  | {
      request: BundleImportRequest;
      type: "ImportBundle";
    }
  | {
      limit?: number | null;
      query: string;
      repository: string;
      type: "SearchWorkspaceFiles";
    }
  | {
      internal?: boolean;
      parent_run_id?: string | null;
      parent_session_id?: string | null;
      /**
       * The canonical filesystem root of the repository this session operates on, so the daemon can build its code graph on open (not only on the first run). `#[serde(default)]` keeps older clients (which send none) working.
       */
      repository?: string | null;
      title: string;
      type: "CreateSession";
      workspace: string;
    }
  | {
      session_id: string;
      type: "CloseSession";
    }
  | {
      last_seen_sequence?: number | null;
      /**
       * The canonical filesystem root of the repository this session operates on, so the daemon can build its code graph on open (not only on the first run). `#[serde(default)]` keeps older clients (which send none) working.
       */
      repository?: string | null;
      requested_role: ClientRole;
      session_id: string;
      subscriptions: Subscription[];
      type: "AttachSession";
    }
  | {
      /**
       * The full multimodal input this submission normalizes (voice v1, rubric 8): a typed [`InputEnvelope`] whose blocks may reference artifacts previously stored via [`PutArtifact`](CommandBody::PutArtifact) — e.g. an [`InputBlock::Audio`](crate::input::InputBlock::Audio) carrying a recorded voice note. When the envelope carries audio without a transcript, the daemon transcribes it (through its transcription seam, gated by the [`transcription_allowed`](crate::input::transcription_allowed) classification math) and the transcript text becomes the run input; the original audio stays linked to its transcript (the original-is-never-replaced invariant). Additive (`#[serde(default)]`): an older client omits it and `text` alone drives the run, exactly as before this field existed.
       */
      envelope?: InputEnvelope | null;
      mode: AgentMode;
      /**
       * The model to **pin** this continuation to (mid-conversation model switch). When the operator re-picks a model in the `/model` picker, the very next follow-up in the SAME session carries it here so the switch is instant — no restart, no new session. `Some(id)` runs this continuation on exactly that model AND makes it the session's current pin (a later follow-up that carries none inherits it via [`session_run_provenance`](crate) recovery). `None` is unchanged behavior: the continuation inherits the session's existing model from its originating `StartRun`. Mirrors [`StartRun.model`](CommandBody::StartRun::model): `#[serde(default)]` keeps an older client (which sends none) working — the daemon then resolves the model exactly as before this field existed.
       */
      model?: string | null;
      session_id: string;
      text: string;
      type: "SubmitUserInput";
    }
  | {
      mode: AgentMode;
      /**
       * The model to **pin** this run to (STEP MP2): when the operator picks a model in the `/model` picker, the run executes on exactly that model instead of the router's/resolver's choice. A pin overrides the daemon's *quality* judgment, never its *security* constraint — a pinned hosted model for classified data is refused (fail-closed), never silently run off-device (enforced in the executor). Mirrors [`repository`](CommandBody::StartRun::repository): `#[serde(default)]` keeps an older client (which sends none) working — the daemon then resolves the model exactly as before this field existed.
       */
      model?: string | null;
      objective: string;
      /**
       * The canonical filesystem root of the repository this run operates on. A per-user daemon can serve several checkouts over one socket, so the run — not the daemon's startup working directory — must decide which repository its context map and curated memories are attributed to (issue #6 item 1). `#[serde(default)]` keeps an older client (which sends none) working: the daemon then falls back to its own directory, exactly as before this field existed.
       */
      repository?: string | null;
      session_id: string;
      type: "StartRun";
    }
  | {
      approval_id: string;
      decision: ApprovalDecision;
      scope: ApprovalScope;
      type: "ResolveApproval";
    }
  | {
      outcome: QuestionOutcome;
      question_id: string;
      type: "ResolveQuestion";
    }
  | {
      run_id: string;
      type: "CancelRun";
    }
  | {
      run_id: string;
      type: "PauseRun";
    }
  | {
      run_id: string;
      type: "ResumeRun";
    }
  | {
      run_id: string;
      text: string;
      type: "QueueSteering";
    }
  | {
      session_id: string;
      type: "UpdateIdeContext";
      update: IdeContextUpdate;
    }
  | {
      /**
       * Markdown to seed the document's blocks from (`docs new --from file.md`, the agent's `docs.create`). Imported lossily-but- reasonably at block granularity; absent creates an empty document.
       */
      initial_markdown?: string | null;
      /**
       * The canonical filesystem root of the repository a repository-scoped document belongs to. Mirrors [`StartRun.repository`](CommandBody::StartRun::repository): `#[serde(default)]` keeps an older client working — the daemon then falls back to its own startup root.
       */
      repository?: string | null;
      /**
       * The scope to create the document in: `"repository"` (the default when absent — the document lives with the checkout), `"system"`, or `"organization:<id>"` (organization docs default to suggest-only agent collaboration). An unrecognized value is rejected `document.invalid-scope`, never guessed at.
       */
      scope?: string | null;
      /**
       * The document title (non-empty; the daemon rejects a blank one).
       */
      title: string;
      type: "CreateDocument";
    }
  | {
      /**
       * The canonical filesystem root of the repository whose code graph the links resolve against. Mirrors [`StartRun.repository`](CommandBody::StartRun::repository); absent falls back to the daemon's startup root.
       */
      repository?: string | null;
      /**
       * A session to surface the result into: when set and the sweep found anything stale, the daemon appends a `NoteAppended` to this session's ledger so the finding count reaches the active conversation. Absent, the counts ride only on the reply.
       */
      session_id?: string | null;
      type: "CheckDocuments";
    }
  | {
      document_id: string;
      mutation: DocumentMutation;
      type: "MutateDocument";
    }
  | {
      lease: DocumentEditLease;
      /**
       * How long the lease is valid, in seconds; the daemon applies a default when absent. A re-acquire by the same holder renews the expiry in place.
       */
      ttl_seconds?: number | null;
      type: "AcquireDocumentLease";
    }
  | {
      lease_id: string;
      type: "ReleaseDocumentLease";
    }
  | {
      document_id: string;
      target: PublishTarget;
      type: "PublishDocument";
    }
  | {
      inputs?: JsonValue;
      /**
       * The workflow manifest YAML (the content of a `workflow.yaml`). Empty when [`workflow_id`](CommandBody::StartWorkflow::workflow_id) names a workflow the daemon resolves from its own sources instead.
       */
      manifest: string;
      /**
       * The canonical filesystem root of the repository this workflow's agent nodes operate on. A per-user daemon can serve several checkouts over one socket, so the run — not the daemon's startup working directory — must decide which repository its agent nodes' isolated worktrees are carved from (Phase 5 T5, fixing P5-D1). Mirrors [`StartRun.repository`](CommandBody::StartRun): `#[serde(default)]` keeps an older client (which sends none) working — the daemon then falls back to its own startup repository root, never a wandering `current_dir()` at node-execution time.
       */
      repository?: string | null;
      type: "StartWorkflow";
      /**
       * A named workflow to resolve from the daemon's sources (embedded built-ins, the user config directory, and the run repository's `.codypendent/workflows`) rather than shipping the manifest inline — the path `/fix-ci` takes (`repair-github-check`). Additive (`#[serde(default)]`): an older client omits it and ships `manifest`. When set, `manifest` is ignored and the daemon enforces the workflow registry's version-stability + shadowing rules at resolution.
       */
      workflow_id?: string | null;
    }
  | {
      type: "PauseWorkflow";
      workflow_run_id: string;
    }
  | {
      type: "ResumeWorkflow";
      workflow_run_id: string;
    }
  | {
      /**
       * The node id to re-drive from (its transitive dependents reset with it).
       */
      node_id: string;
      type: "RetryWorkflowNode";
      workflow_run_id: string;
    }
  | {
      type: "CancelWorkflow";
      workflow_run_id: string;
    }
  | {
      type: "ReadWorkflowRun";
      workflow_run_id: string;
    }
  | {
      kind: string;
      name: string;
      requires_permission_review?: boolean;
      type: "ProposePromotion";
      version: number;
    }
  | {
      action: PromotionAction;
      candidate_id: string;
      type: "AdvancePromotion";
    }
  | {
      candidate_id: string;
      type: "ApprovePromotion";
    }
  | {
      candidate_id: string;
      type: "RollbackPromotion";
    }
  | {
      /**
       * Read a **repository task board** instead of a workflow run's board (Phase B kanban). When set, `workflow_run_id` is ignored: the daemon resolves the board to the synthetic run [`board_scope_id`](crate::blackboard::board_scope_id) names (an empty board for a repository never written to — a read creates nothing). Additive (`#[serde(default)]`): an older client omits it and reads a run board exactly as before.
       */
      board_repository?: string | null;
      /**
       * Include superseded revisions too; the default (`false`) returns only the live board (the "live-only" view the TUI shows).
       */
      include_superseded?: boolean;
      /**
       * A blackboard artifact kind to filter by (`finding`, `decision`, …), or all kinds when absent.
       */
      kind?: string | null;
      type: "ReadBlackboard";
      workflow_run_id: string;
    }
  | {
      /**
       * The artifact to store (kind, payload, evidence, board fields).
       */
      item: BlackboardItemDraft;
      /**
       * The board to post onto: a workflow run's, or a repository's task board (created on first write).
       */
      scope: BlackboardScope;
      type: "PostBlackboardItem";
    }
  | {
      /**
       * The new assignee, when re-assigning.
       */
      assignee?: string | null;
      /**
       * The live item to supersede. An already-superseded item is refused (`blackboard.already-superseded`), so concurrent moves never fork.
       */
      item_id: string;
      /**
       * The new within-column position, when re-ordering. When only `status` changes, the daemon appends to the end of the target column.
       */
      ordinal?: number | null;
      payload?: JsonValue;
      /**
       * The board holding the item.
       */
      scope: BlackboardScope;
      /**
       * The new column, when moving.
       */
      status?: string | null;
      type: "UpdateBlackboardItem";
    }
  | {
      /**
       * Return events strictly **after** this sequence (0 = from the start).
       */
      after_sequence?: number;
      /**
       * Maximum events in the page. 0 (or absent) asks for the server default; the server clamps any request to its own page ceiling.
       */
      limit?: number;
      session_id: string;
      type: "ReadSessionEvents";
    }
  | {
      id: string;
      repository: string;
      type: "InspectMemory";
    }
  | {
      confidence: number;
      id: string;
      repository: string;
      statement: string;
      structured_value?: JsonValue;
      type: "CorrectMemory";
    }
  | {
      id: string;
      repository: string;
      type: "ForgetMemory";
    }
  | {
      repository: string;
      tier: MemoryScopeTier;
      type: "ForgetMemoryScope";
    }
  | {
      evidence_index: number;
      id: string;
      repository: string;
      type: "OpenMemoryEvidence";
    }
  | {
      candidate_id: string;
      /**
       * A serialized `codypendent_eval::SuiteReport`. Opaque here: protocol must not depend on `codypendent-eval`.
       */
      report_json: string;
      /**
       * The routing policy the suite ran under, or `daemon-default`.
       */
      routing_policy: string;
      /**
       * The eval suite that ran. The regression gate consumes `core`.
       */
      suite: string;
      type: "SubmitEvalEvidence";
    }
  | {
      /**
       * The raw bytes, base64-encoded (standard alphabet, with padding).
       */
      bytes_base64: string;
      /**
       * IANA media type of the bytes, e.g. `audio/wav`.
       */
      media_type: string;
      /**
       * The stored occurrence's data classification.
       */
      sensitivity: DataClassification;
      type: "PutArtifact";
    }
  | {
      artifact_id: string;
      expected_sha256: string;
      limit: number;
      offset: number;
      type: "ReadArtifact";
    }
  | {
      /**
       * The directory to build from; resolved to its enclosing checkout.
       */
      repository: string;
      type: "BuildCodeGraph";
    }
  | {
      repository: string;
      type: "ReadCodeGraphStatus";
    }
  | {
      /**
       * The `models.toml` id to probe, or every configured model when absent.
       */
      model?: string | null;
      /**
       * Whether the daemon may reach the provider over the network.
       *
       * `false` — the default, and what a page full of rows should ask for — resolves credentials only. `true` is the per-row "Test": it costs a request to the provider and proves the model is listed. Never implied, because a client refreshing a list must not be able to make the daemon spend a request per model without saying so.
       */
      network?: boolean;
      type: "ProbeModel";
    }
  | {
      query?: CodeGraphQuery;
      repository: string;
      type: "ReadCodeGraph";
    }
  | {
      checkpoint: string;
      run_id: string;
      type: "RestoreCheckpoint";
    }
  | {
      checkpoint: string;
      /**
       * The fork's title; absent derives `"<source title> (fork)"` with an opencode-style `#N` auto-increment on collision.
       */
      name?: string | null;
      session_id: string;
      type: "ForkSession";
    }
  | {
      delivery: PromptDelivery;
      mode: AgentMode;
      session_id: string;
      text: string;
      type: "QueuePrompt";
    }
  | {
      delivery?: PromptDelivery | null;
      prompt_id: string;
      session_id: string;
      text?: string | null;
      type: "UpdateQueuedPrompt";
    }
  | {
      prompt_id: string;
      session_id: string;
      type: "PromoteQueuedPrompt";
    }
  | {
      prompt_id: string;
      session_id: string;
      type: "DeleteQueuedPrompt";
    }
  | {
      command: string;
      session_id: string;
      type: "RunUserShell";
    }
  | {
      session_id: string;
      text: string;
      type: "RememberMemory";
    }
  | {
      limit?: number | null;
      query: string;
      type: "MarketplaceSearch";
    }
  | {
      allow_unsigned?: boolean;
      artifact_base64?: string | null;
      manifest_toml?: string | null;
      package_id: string;
      type: "MarketplaceInstall";
    }
  | {
      allow_unsigned?: boolean;
      artifact_base64?: string | null;
      manifest_toml?: string | null;
      package_id: string;
      type: "MarketplaceUpdate";
    }
  | {
      package_id: string;
      scope?: string | null;
      session_id?: string | null;
      type: "MarketplaceEnable";
    }
  | {
      package_id: string;
      type: "MarketplaceDisable";
    }
  | {
      package_id: string;
      reason?: string;
      type: "MarketplaceRevoke";
    }
  | {
      backend: string;
      capability: string;
      locator: string;
      name: string;
      organization_id?: string | null;
      repository_id?: string | null;
      type: "SecretDeclare";
    }
  | {
      capability: string;
      job_id: string;
      reference_id: string;
      type: "SecretBind";
    }
  | {
      capability?: string | null;
      type: "SecretList";
    }
  | {
      reason?: string;
      reference_id: string;
      type: "SecretRevoke";
    }
  | {
      display_name?: string | null;
      repository: string;
      type: "EstablishFederatedIdentity";
    }
  | {
      repository: string;
      type: "GetPublicationPolicy";
    }
  | {
      policy: UpdatePublicationPolicyRequest;
      repository: string;
      type: "SetPublicationPolicy";
    }
  | {
      idempotency_key?: string;
      repository: string;
      type: "PublishGraphFacts";
    }
  | {
      reason: string;
      repository: string;
      subject_id: string;
      subject_kind: string;
      type: "TombstoneGraphFacts";
    }
  | {
      query?: FederatedGraphQuery;
      type: "QueryFederatedGraph";
    }
  | {
      query: BlastRadiusQuery;
      type: "QueryBlastRadius";
    }
  | {
      query: MigrationPlanQuery;
      type: "PlanMigration";
    }
  | {
      query: ReviewerSuggestionQuery;
      type: "SuggestReviewers";
    }
  | {
      campaign: CreateCampaignRequest;
      type: "CreateCampaign";
    }
  | {
      campaign_id: string;
      type: "GetCampaign";
    }
  | {
      limit?: number | null;
      state?: CampaignState | null;
      type: "ListCampaigns";
    }
  | {
      request: ExecuteCampaignRequest;
      type: "ExecuteCampaign";
    }
  | {
      campaign_id: string;
      type: "CancelCampaign";
    }
  | {
      type: "Unknown";
    };
/**
 * The lifecycle state of a run (Chapter 04). Transitions are persisted before they are exposed to clients.
 */
type RunState =
  | {
      type: "Queued";
    }
  | {
      type: "Preparing";
    }
  | {
      type: "Running";
    }
  | {
      type: "WaitingForApproval";
    }
  | {
      type: "WaitingForUserInput";
    }
  | {
      type: "Paused";
    }
  | {
      type: "Recovering";
    }
  | {
      type: "Completed";
    }
  | {
      type: "Failed";
    }
  | {
      type: "Cancelled";
    }
  | {
      type: "Unknown";
    };
/**
 * A lifecycle mutation. The containing command supplies the idempotency key.
 */
type SessionLifecycleAction =
  | {
      title: string;
      type: "Rename";
    }
  | {
      type: "Pin";
    }
  | {
      type: "Unpin";
    }
  | {
      type: "Archive";
    }
  | {
      type: "Restore";
    }
  | {
      mode?: SessionDeletionMode;
      type: "Delete";
    }
  | {
      options: SessionExportOptions;
      type: "Export";
    }
  | {
      type: "Unknown";
    };
/**
 * Retention behavior requested by a session deletion. The daemon remains the policy authority and may reject a mode rather than weakening retention.
 */
type SessionDeletionMode =
  | {
      type: "RetentionPolicy";
    }
  | {
      type: "TombstoneOnly";
    }
  | {
      type: "Unknown";
    };
/**
 * Portable session export formats understood by clients.
 */
type SessionExportFormat =
  | {
      type: "Json";
    }
  | {
      type: "Markdown";
    }
  | {
      type: "Unknown";
    };
/**
 * An ordinary run entry point contributed by an editor client.
 */
type EditorNativeAction =
  | {
      type: "FixSelection";
    }
  | {
      type: "ExplainSelection";
    }
  | {
      type: "ReviewCurrentFile";
    }
  | {
      type: "GenerateTestsForSelection";
    }
  | {
      diagnostic: Diagnostic;
      type: "FixDiagnostic";
    }
  | {
      type: "Unknown";
    };
/**
 * Severity of an editor diagnostic, mirroring the common LSP levels.
 */
type DiagnosticSeverity =
  | {
      type: "Error";
    }
  | {
      type: "Warning";
    }
  | {
      type: "Information";
    }
  | {
      type: "Hint";
    }
  | {
      type: "Unknown";
    };
/**
 * The human work or notification represented by an inbox entry.
 */
type InboxEntryKind =
  | {
      type: "ApprovalRequest";
    }
  | {
      type: "AgentQuestion";
    }
  | {
      type: "RunCompleted";
    }
  | {
      type: "RunFailed";
    }
  | {
      type: "BudgetWarning";
    }
  | {
      type: "WorkflowBlocked";
    }
  | {
      type: "PluginPermissionChanged";
    }
  | {
      type: "RunnerFailed";
    }
  | {
      type: "Unknown";
    };
/**
 * Read/lifecycle state of a durable inbox entry.
 */
type InboxEntryState =
  | {
      type: "Unread";
    }
  | {
      type: "Acknowledged";
    }
  | {
      type: "Dismissed";
    }
  | {
      type: "Resolved";
    }
  | {
      type: "Unknown";
    };
/**
 * Idempotent state change requested for an inbox entry.
 */
type InboxMutation =
  | {
      entry_id: string;
      type: "Acknowledge";
    }
  | {
      entry_id: string;
      type: "Dismiss";
    }
  | {
      type: "Unknown";
    };
/**
 * Completion outcome used both as a filter and an aggregate dimension.
 */
type AnalyticsCompletion =
  | {
      type: "successful";
    }
  | {
      type: "failed";
    }
  | {
      type: "cancelled";
    }
  | {
      type: "incomplete";
    }
  | {
      type: "unknown";
    };
/**
 * Dimensions by which observations may be grouped.
 */
type AnalyticsGrouping =
  | {
      type: "model";
    }
  | {
      type: "provider";
    }
  | {
      type: "repository";
    }
  | {
      type: "workflow";
    }
  | {
      type: "task_class";
    }
  | {
      type: "time";
    }
  | {
      type: "completion";
    }
  | {
      type: "route";
    }
  | {
      type: "unknown";
    };
/**
 * Supported analytics export encodings. The request must choose explicitly.
 */
type AnalyticsExportFormat =
  | {
      type: "json";
    }
  | {
      type: "csv";
    }
  | {
      type: "unknown";
    };
/**
 * Normalized CRUD requests. The containing command provides idempotency.
 */
type AutomationBindingRequest =
  | {
      binding: AutomationBindingDraft;
      type: "create";
    }
  | {
      id: string;
      type: "get";
    }
  | {
      query: AutomationBindingQuery;
      type: "list";
    }
  | {
      id: string;
      patch: AutomationBindingPatch;
      type: "update";
    }
  | {
      id: string;
      type: "delete";
    }
  | {
      type: "unknown";
    };
type AutomationApprovalMode =
  | ("inherit" | "always_require" | "policy_driven" | "unknown")
  | {
      preapproved: {
        approval_receipt: string;
      };
    };
type ConcurrencyPolicy = "allow" | "skip" | "queue" | "replace" | "unknown";
type MissedRunPolicy =
  | ("skip" | "run_once" | "unknown")
  | {
      catch_up: {
        max_occurrences: number;
      };
    };
/**
 * The event or schedule that can invoke a binding.
 */
type TriggerSource =
  | {
      expression: string;
      timezone: string;
      type: "cron";
    }
  | {
      at: string;
      type: "one_time";
    }
  | {
      endpoint_id: string;
      events?: string[];
      installation_id?: number | null;
      type: "git_hub_webhook";
    }
  | {
      endpoint_id: string;
      signature: WebhookSignatureScheme;
      /**
       * Reference to daemon-owned secret material, never the secret itself.
       */
      signing_key_ref: string;
      type: "signed_webhook";
    }
  | {
      provider?: string | null;
      type: "ci_failure";
      workflows?: string[];
    }
  | {
      type: "repository_change";
    }
  | {
      type: "code_graph_change";
    }
  | {
      ecosystems?: string[];
      type: "dependency_alert";
    }
  | {
      type: "manual";
    }
  | {
      type: "api";
    }
  | {
      type: "unknown";
    };
type WebhookSignatureScheme = "hmac_sha256" | "ed25519" | "unknown";
/**
 * Normalized budget CRUD. The containing command provides idempotency.
 */
type AnalyticsBudgetRequest =
  | {
      budget: AnalyticsBudgetDraft;
      type: "create";
    }
  | {
      id: string;
      type: "get";
    }
  | {
      query: AnalyticsBudgetQuery;
      type: "list";
    }
  | {
      id: string;
      patch: AnalyticsBudgetPatch;
      type: "update";
    }
  | {
      id: string;
      type: "delete";
    }
  | {
      type: "unknown";
    };
/**
 * The measured dimension a budget threshold applies to.
 *
 * Deliberately a CLOSED set of *measured* dimensions, matching the migration's `CHECK (dimension IN (...))`. A budget over an unmeasured dimension would have to read `NULL` as `0` to decide anything, which is exactly the zero-coercion the measurement contract forbids — so those dimensions are not expressible here at all.
 */
type AnalyticsBudgetDimension =
  | {
      type: "cost_micros";
    }
  | {
      type: "input_tokens";
    }
  | {
      type: "output_tokens";
    }
  | {
      type: "latency_ms";
    }
  | {
      type: "unknown";
    };
/**
 * What a budget is scoped to. `Owner` covers everything the principal runs; the others narrow to one repository, workflow, or model.
 *
 * The storage layer splits this into `(scope, scope_value)` because 0043 does — `scope_value` is `''` for `Owner` — but the wire contract keeps the value attached to the scope that gives it meaning, so a repository id can never be paired with `scope = 'model'`.
 */
type AnalyticsBudgetScope =
  | {
      type: "owner";
    }
  | {
      repository_id: string;
      type: "repository";
    }
  | {
      type: "workflow";
      workflow_id: string;
    }
  | {
      model_id: string;
      type: "model";
    }
  | {
      type: "unknown";
    };
/**
 * The rolling window a budget's threshold is measured over.
 */
type AnalyticsBudgetWindow =
  | {
      type: "day";
    }
  | {
      type: "week";
    }
  | {
      type: "month";
    }
  | {
      type: "unknown";
    };
/**
 * Redactions an exporter must perform before hashing archive entries.
 */
type BundleRedactionPolicy =
  | {
      type: "Standard";
    }
  | {
      type: "SupportSafe";
    }
  | {
      type: "Unknown";
    };
/**
 * How sensitive an artifact's contents are.
 *
 * Ordered least to most restrictive; higher classifications gate model routing, export, and display. A wire enum, so it is internally tagged and carries an [`DataClassification::Unknown`] fallback for forward compatibility.
 */
type DataClassification =
  | {
      type: "Public";
    }
  | {
      type: "Internal";
    }
  | {
      type: "Confidential";
    }
  | {
      type: "Secret";
    }
  | {
      type: "Unknown";
    };
/**
 * How an importer handles a source identity that already exists locally.
 */
type BundleCollisionPolicy =
  | {
      type: "Reject";
    }
  | {
      type: "Remap";
    }
  | {
      type: "Skip";
    }
  | {
      type: "Unknown";
    };
/**
 * A client's authority over a session it observes (Chapter 03). Exclusivity is attached to specific resources (leases), not to the whole session.
 */
type ClientRole =
  | {
      type: "Observer";
    }
  | {
      type: "Contributor";
    }
  | {
      type: "Controller";
    }
  | {
      type: "Approver";
    }
  | {
      type: "Unknown";
    };
/**
 * A projection view a client subscribes to, rather than receiving every internal event (Chapter 03). This is the Phase 1 subset; document, workflow, and GitHub views arrive with their features.
 */
type Subscription =
  | {
      type: "SessionSummary";
    }
  | {
      run_id: string;
      type: "RunTrace";
    }
  | {
      type: "AgentActivity";
    }
  | {
      type: "RepositoryStatus";
    }
  | {
      type: "BudgetState";
    }
  | {
      document_id: string;
      type: "Document";
    }
  | {
      type: "Blackboard";
      workflow_run_id: string;
    }
  | {
      type: "Workflow";
      workflow_run_id: string;
    }
  | {
      type: "Unknown";
    };
/**
 * One typed block within an [`InputEnvelope`]. Internally tagged on the wire with a `block` discriminant (`{"block": "...", …fields}`) so the media variants carry structured, artifact-linked payloads inline; an unrecognized block decodes to [`InputBlock::Unknown`] for forward compatibility. The discriminant is `block` (not `kind`) precisely because inner payloads such as [`SymbolRef`]/[`GitHubReference`] carry their own `kind` field.
 */
type InputBlock =
  | {
      block: "text";
      text: string;
    }
  | {
      block: "audio";
      duration_ms?: number | null;
      /**
       * The preserved original audio blob (kept where policy allows).
       */
      original: ArtifactRef;
      sample_rate_hz?: number | null;
      /**
       * The transcript, once produced and (for submission) reviewed.
       */
      transcript?: Transcript | null;
    }
  | {
      block: "image";
      /**
       * (2) Extracted text (OCR), when produced. A separate artifact, not a substitute for the image.
       */
      extracted_text?: ArtifactRef | null;
      height?: number | null;
      /**
       * (3) Model observations about the image.
       */
      observations?: ModelObservation[];
      /**
       * (1) The original image — never replaced by a summary.
       */
      original: ArtifactRef;
      /**
       * (4) Crop / coordinate references into the image.
       */
      regions?: ImageRegion[];
      width?: number | null;
    }
  | {
      block: "file";
      byte_length: number;
      id: string;
      /**
       * IANA media type, e.g. `text/plain` or `application/json`.
       */
      media_type: string;
      sensitivity: DataClassification;
      /**
       * Lowercase hex SHA-256 of the blob's bytes (the content address).
       */
      sha256: string;
    }
  | {
      block: "editor-selection";
      path: string;
      range: Range;
    }
  | {
      block: "code-symbol";
      /**
       * The symbol kind (`function`, `struct`, …), when known.
       */
      kind?: string | null;
      line?: number | null;
      path: string;
      /**
       * The symbol name (e.g. `WorkflowDriver::advance`).
       */
      symbol: string;
    }
  | {
      block: "github-reference";
      kind: GitHubRefKind;
      /**
       * The PR/issue number, or `None` for a commit/repo reference.
       */
      number?: number | null;
      owner: string;
      repo: string;
      url?: string | null;
    }
  | {
      block: "unknown";
    };
/**
 * Where a transcription (or any media interpretation) runs.
 */
type TranscriptionMode =
  | {
      type: "local";
    }
  | {
      type: "remote";
    }
  | {
      type: "unknown";
    };
/**
 * The kind of GitHub entity a [`GitHubReference`] points to.
 */
type GitHubRefKind =
  | {
      type: "pull-request";
    }
  | {
      type: "issue";
    }
  | {
      type: "commit";
    }
  | {
      type: "comment";
    }
  | {
      type: "unknown";
    };
/**
 * The scope hierarchy an input applies at (README: `System → Organisation → User → Workspace → Repository → Branch → Session → Task`).
 */
type ScopeLevel =
  | {
      type: "system";
    }
  | {
      type: "organization";
    }
  | {
      type: "user";
    }
  | {
      type: "workspace";
    }
  | {
      type: "repository";
    }
  | {
      type: "branch";
    }
  | {
      type: "session";
    }
  | {
      type: "task";
    }
  | {
      type: "unknown";
    };
/**
 * Where an input originated.
 */
type InputSource =
  | {
      type: "tui";
    }
  | {
      type: "ide";
    }
  | {
      type: "cli";
    }
  | {
      type: "web";
    }
  | {
      type: "voice";
    }
  | {
      type: "unknown";
    };
/**
 * A mode preset: a bundle of policy and interaction defaults, not merely a prompt (Chapter 20). Modes are enforced by the policy engine — an `Explore` run proposing a write is denied regardless of what the model says.
 */
type AgentMode =
  | {
      type: "Ask";
    }
  | {
      type: "Explore";
    }
  | {
      type: "Plan";
    }
  | {
      type: "Build";
    }
  | {
      type: "Review";
    }
  | {
      type: "Unknown";
    };
/**
 * The decision an approver returns for a proposed action.
 */
type ApprovalDecision =
  | {
      type: "Approve";
    }
  | {
      type: "Reject";
    }
  | {
      type: "Unknown";
    };
/**
 * How widely an approval applies (Chapter 04 / STEP 1.6).
 */
type ApprovalScope =
  | {
      type: "Once";
    }
  | {
      type: "Run";
    }
  | {
      type: "Pattern";
    }
  | {
      type: "Repository";
    }
  | {
      type: "Unknown";
    };
/**
 * How a question was resolved.
 */
type QuestionOutcome =
  | {
      answers: string[][];
      type: "Answered";
    }
  | {
      feedback?: string | null;
      type: "Rejected";
    }
  | {
      type: "Unknown";
    };
/**
 * A semantic mutation on a collaborative document. Internally tagged with an [`DocumentMutation::Unknown`] fallback so a newer client's mutation deserializes and is rejected structurally rather than crashing the peer.
 */
type DocumentMutation =
  | {
      block_id: string;
      content: JsonValue;
      index: number;
      op: "insert";
    }
  | {
      block_id: string;
      op: "delete";
    }
  | {
      block_id: string;
      delete_len?: number;
      insert?: string;
      op: "edit_text";
      position: number;
    }
  | {
      op: "annotate";
      suggestion: SuggestionInput;
    }
  | {
      op: "accept_suggestion";
      suggestion_id: string;
    }
  | {
      op: "reject_suggestion";
      suggestion_id: string;
    }
  | {
      op: "unknown";
    };
/**
 * Where a document publish writes (Phase 4 STEP 4.4). Wire mirror of `codypendent-knowledge`'s `PublishTarget` domain type — the protocol crate cannot name the knowledge crate, so this carries the same three targets across the wire and the `codypendentd` assembly converts one into the other before computing the plan. Internally tagged with a [`PublishTarget::Unknown`] fallback so a target a newer peer added deserializes and is rejected structurally rather than crashing the peer.
 */
type PublishTarget =
  | {
      kind: "repository_file";
      path: string;
    }
  | {
      branch: string;
      kind: "docs_branch_commit";
      path: string;
    }
  | {
      branch: string;
      kind: "documentation_pr";
      path: string;
      title: string;
    }
  | {
      kind: "unknown";
    };
/**
 * One legal state-machine transition to attempt via `AdvancePromotion` (Phase 7 STEP 7.5). Mirrors `codypendent_eval::promote::Candidate`'s methods exactly. Regression and canary verdicts are computed from durable evidence by the daemon, never supplied as client booleans.
 */
type PromotionAction =
  | {
      type: "RunRegression";
    }
  | {
      type: "ReviewPermissions";
    }
  | {
      type: "StartShadow";
    }
  | {
      type: "StartCanary";
    }
  | {
      type: "ObserveCanary";
    }
  | {
      type: "FinishCanary";
    }
  | {
      type: "Unknown";
    };
/**
 * Which durable board a client-side blackboard write targets: a workflow run's board, or a repository's task board (the kanban surface). A wire enum with an [`Unknown`](BlackboardScope::Unknown) fallback so a scope from a newer client is rejected structurally rather than failing the frame.
 */
type BlackboardScope =
  | {
      type: "WorkflowRun";
      workflow_run_id: string;
    }
  | {
      repository: string;
      type: "RepositoryBoard";
    }
  | {
      type: "Unknown";
    };
/**
 * Which of the caller's *visible* scopes a bulk forget targets. Deliberately not a scope key: see the module doc.
 */
type MemoryScopeTier =
  | {
      type: "System";
    }
  | {
      type: "User";
    }
  | {
      type: "Repository";
    }
  | {
      type: "Unknown";
    };
/**
 * How a pending prompt is delivered (Adoption 06, cline's `PendingPromptDelivery`). `Steer` feeds the live run's steering channel at its next safe point; `Queue` waits for the session to go idle and launches a continuation run.
 */
type PromptDelivery =
  | {
      type: "Queue";
    }
  | {
      type: "Steer";
    }
  | {
      type: "Unknown";
    };
/**
 * Audience breadth of a published fact or repository policy ceiling.
 */
type PublicationClass =
  "private-local" | "metadata-shared" | "content-shared" | "organization-knowledge" | "public-marketplace" | "unknown";
/**
 * Kind of multi-repository campaign or migration plan.
 */
type CampaignKind =
  "api-migration" | "schema-migration" | "dependency-upgrade" | "ownership-review" | "custom" | "unknown";
/**
 * Approval mode for a campaign repository enrollment.
 */
type CampaignApprovalMode = "per-effect" | "per-run" | "unknown";
/**
 * Lifecycle state of a coordinated campaign.
 */
type CampaignState = "planning" | "running" | "partially-failed" | "completed" | "cancelled" | "unknown";
/**
 * A machine-readable hint about how the user could resolve an error, so the client can render the right affordance instead of parsing `message`.
 */
type UserAction =
  | {
      type: "Retry";
    }
  | {
      type: "Reauthenticate";
    }
  | {
      type: "GrantApproval";
    }
  | {
      type: "AdjustPolicy";
    }
  | {
      type: "ReconfigureModel";
    }
  | {
      type: "ContactSupport";
    }
  | {
      type: "Unknown";
    };
/**
 * Stable navigation target for a session-library result.
 */
type SessionDeepLink =
  | {
      session_id: string;
      type: "Session";
    }
  | {
      run_id: string;
      session_id: string;
      type: "Run";
    }
  | {
      sequence: number;
      session_id: string;
      type: "Event";
    }
  | {
      artifact_id: string;
      session_id: string;
      type: "Artifact";
    }
  | {
      column?: number | null;
      line?: number | null;
      path: string;
      session_id: string;
      type: "Path";
    }
  | {
      path?: string | null;
      session_id: string;
      symbol: string;
      type: "Symbol";
    }
  | {
      type: "Unknown";
    };
/**
 * Authorization scope in which a search hit was found.
 */
type SessionSearchScope =
  | {
      type: "Session";
    }
  | {
      type: "Repository";
    }
  | {
      type: "Workspace";
    }
  | {
      type: "User";
    }
  | {
      type: "Unknown";
    };
/**
 * The indexed material responsible for a search hit.
 */
type SessionSearchSource =
  | {
      type: "Title";
    }
  | {
      type: "Transcript";
    }
  | {
      type: "ToolObservation";
    }
  | {
      type: "Patch";
    }
  | {
      type: "Artifact";
    }
  | {
      type: "ChangedPath";
    }
  | {
      type: "Symbol";
    }
  | {
      type: "Unknown";
    };
type Actor =
  | {
      type: "Human";
      user_id: string;
    }
  | {
      agent_id: string;
      model: string;
      run_id: string;
      type: "Agent";
    }
  | {
      client_id: string;
      type: "Client";
    }
  | {
      integration_id: string;
      type: "Integration";
    }
  | {
      type: "System";
    }
  | {
      type: "Unknown";
    };
/**
 * The body of a persisted event.
 *
 * Internally tagged with a `#[serde(other)] Unknown` fallback (RULE 1): an event type produced by a newer daemon deserializes to `Unknown` in an older client instead of failing the whole frame, and the client renders an "unsupported item" placeholder. Phase 0 variants are preserved so old ledger bytes parse forever.
 */
type EventBody =
  | {
      title: string;
      type: "SessionCreated";
    }
  | {
      /**
       * The run this note belongs to, when it is run-scoped (a run's context manifest or a curated-memory note). `None` for a session-level note (e.g. user input, an effect-reconciliation record), which a client attaches to whatever run is in focus. Without this, a run's note could land on the wrong transcript when runs interleave (issue #6 item 3). `#[serde(default)]` keeps old ledger bytes (which have no `run_id`) parsing to `None`.
       */
      run_id?: string | null;
      text: string;
      type: "NoteAppended";
    }
  | {
      type: "SessionClosed";
    }
  | {
      mode: AgentMode;
      objective: string;
      run_id: string;
      type: "RunStarted";
    }
  | {
      run_id: string;
      state: RunState;
      type: "RunStateChanged";
    }
  | {
      run_id: string;
      text: string;
      /**
       * `true` when this chunk is reasoning, not reply. Defaults to `false` so a payload written before this field existed still parses.
       */
      thought?: boolean;
      type: "ModelStreamDelta";
    }
  | {
      attempt: number;
      /**
       * The wait before the retry fires, in milliseconds.
       */
      delay_ms: number;
      max_attempts: number;
      /**
       * Bounded classifier reason (e.g. "provider is overloaded").
       */
      message: string;
      run_id: string;
      type: "ModelRetrying";
    }
  | {
      action: ProposedAction;
      approval_id: string;
      run_id: string;
      type: "ToolProposed";
    }
  | {
      action: ProposedAction;
      reasons?: string[];
      run_id: string;
      type: "ToolDenied";
    }
  | {
      /**
       * Digest of the tool arguments (not the arguments themselves).
       */
      args_digest: string;
      /**
       * A short, human-readable display label for the call — e.g. the file path a `workspace.read_file` targets, or the command a `shell.run` executes — so a client can render `workspace.read_file · services/main.py` instead of the bare tool name. Derived by the emitter (`codypendent_runtime::tools::tool_label`) from the same arguments `args_digest` hashes, BEFORE they are discarded: bounded, single-line, and never the full arguments or file contents. `#[serde(default)]` keeps old ledger bytes and an older daemon's events (neither carries this field) deserializing to `None` — additive and back-compatible.
       */
      label?: string | null;
      run_id: string;
      /**
       * Tool name, e.g. `shell.run`.
       */
      tool: string;
      type: "ToolStarted";
    }
  | {
      label?: string | null;
      run_id: string;
      tool: string;
      type: "ToolUpdated";
    }
  | {
      /**
       * Bulk output, if any, as an artifact reference.
       */
      artifact?: ArtifactRef | null;
      outcome: ToolOutcome;
      run_id: string;
      tool: string;
      type: "ToolCompleted";
    }
  | {
      /**
       * Added lines in the unified diff.
       */
      additions?: number;
      /**
       * The patch/diff, stored as an artifact.
       */
      artifact: ArtifactRef;
      changeset_id: string;
      /**
       * Removed lines in the unified diff.
       */
      deletions?: number;
      /**
       * Repository-relative paths touched by the change set.
       */
      files?: string[];
      /**
       * A bounded unified-diff preview for immediate review in clients.
       */
      preview?: string;
      /**
       * Whether the full artifact contains more diff than `preview`.
       */
      preview_truncated?: boolean;
      run_id: string;
      type: "PatchProposed";
    }
  | {
      action: ProposedAction;
      approval_id: string;
      pattern?: string | null;
      risk: Risk;
      type: "ApprovalRequested";
    }
  | {
      approval_id: string;
      decision: ApprovalDecision;
      type: "ApprovalResolved";
    }
  | {
      run_id: string;
      type: "SteeringQueued";
    }
  | {
      run_id: string;
      type: "SteeringApplied";
    }
  | {
      dimension: BudgetDimension;
      limit: number;
      run_id: string;
      type: "BudgetWarning";
      used: number;
    }
  | {
      run_id: string;
      system_tokens: number;
      tool_tokens: number;
      transcript_tokens: number;
      type: "ContextUsage";
      used_tokens: number;
      window_tokens: number;
    }
  | {
      /**
       * The run chronicle, stored as a JSON artifact.
       */
      chronicle: ArtifactRef;
      disposition: RunDisposition;
      run_id: string;
      type: "RunCompleted";
    }
  | {
      completion_tokens?: number | null;
      cost_micros?: number | null;
      coverage?: UsageCoverage | null;
      prompt_tokens?: number | null;
      run_id: string;
      type: "RunUsage";
    }
  | {
      activated_count: number;
      activated_ids?: string[];
      proposed_count: number;
      proposed_ids?: string[];
      run_id: string;
      type: "LearningsCaptured";
    }
  | {
      client_id: string;
      /**
       * `true` when the client attached, `false` when it detached.
       */
      present: boolean;
      role: ClientRole;
      type: "ClientPresenceChanged";
    }
  | {
      question_id: string;
      questions: QuestionPrompt[];
      run_id: string;
      type: "QuestionAsked";
    }
  | {
      outcome: QuestionOutcome;
      question_id: string;
      type: "QuestionResolved";
    }
  | {
      /**
       * The commit the run's worktree was carved from — the "state before this turn" restore/fork target for a `commit`-kind checkpoint.
       */
      base_commit: string;
      checkpoint_id: string;
      commit: string;
      kind: CheckpointKind;
      ordinal: number;
      run_id: string;
      type: "CheckpointRecorded";
    }
  | {
      checkpoint_id: string;
      restored: boolean;
      run_id: string;
      type: "CheckpointRestored";
    }
  | {
      checkpoint: string;
      from_session: string;
      type: "SessionForked";
    }
  | {
      prompts: PendingPromptView[];
      type: "PendingPromptsChanged";
    }
  | {
      type: "Unknown";
    };
/**
 * A side-effecting action an agent proposes, pending policy evaluation and possibly approval.
 *
 * This started as the Phase 1 minimal subset of the Chapter 14 shape; Phase 3 adds `GitHubMutation` for remote GitHub writes. Further variants (`InstallPlugin`, structured `CommandRequest` / `NetworkDestination`) arrive in later phases. Paths and destinations are carried as strings on the wire.
 */
type ProposedAction =
  | {
      paths: string[];
      type: "ReadFiles";
    }
  | {
      patch: string;
      type: "WritePatch";
    }
  | {
      args: string[];
      /**
       * The working directory the command runs in, when constrained.
       */
      cwd?: string | null;
      /**
       * The child's *complete* environment as name/value pairs (empty means it inherits nothing). Carried on the action so the approver and the audit ledger see exactly what the command runs with: an unshown, model-controlled environment could otherwise smuggle execution-hijacking variables (`LD_PRELOAD`, `RUSTC_WRAPPER`, a shadowed `PATH`, …) past a benign-looking `run cargo test` approval. Defaulted so an older client that sends none still parses.
       */
      environment?: [string, string][];
      program: string;
      type: "ExecuteCommand";
    }
  | {
      destination: string;
      type: "NetworkRequest";
    }
  | {
      repository: string;
      type: "GitCommit";
    }
  | {
      branch: string;
      remote: string;
      type: "GitPush";
    }
  | {
      /**
       * The `owner/repo` slug the mutation targets.
       */
      repository: string;
      /**
       * A short human-readable description of the write, rendered on the approval card (e.g. `create draft PR on owner/repo`).
       */
      summary: string;
      type: "GitHubMutation";
    }
  | {
      /**
       * The repo-relative files the publish changes.
       */
      changed_files: string[];
      document_id: string;
      /**
       * The resulting Git action (e.g. `commit docs/x.md on branch docs/publish`).
       */
      git_action: string;
      /**
       * A short human description of the target (e.g. `repository file docs/architecture.md`).
       */
      target: string;
      type: "PublishDocument";
    }
  | {
      /**
       * The artifact kind being posted (`finding`, `decision`, …).
       */
      kind: string;
      type: "BlackboardPost";
      /**
       * The workflow run whose board is written (server-derived from the run context, never model-supplied).
       */
      workflow_run_id: string;
    }
  | {
      type: "BlackboardQuery";
      /**
       * The workflow run whose board is read (server-derived).
       */
      workflow_run_id: string;
    }
  | {
      /**
       * The model-supplied arguments as canonical JSON text (a `String`, not a `Value`, so the enum stays `Eq` and the digest is stable).
       */
      args: string;
      /**
       * The server name from `mcp.toml` (server-derived from the tool name's `mcp.<server>.<tool>` prefix, never free-form model text).
       */
      server: string;
      /**
       * A short human-readable description of the call, rendered on the approval card (e.g. `github.create_issue("…")`).
       */
      summary: string;
      /**
       * The tool name on that server (from the server's `tools/list`).
       */
      tool: string;
      type: "McpToolCall";
    }
  | {
      /**
       * Official registry id of the connected agent.
       */
      agent: string;
      /**
       * Canonical, bounded ACP tool-call description.
       */
      details: string;
      /**
       * Human-readable tool/call title reported by ACP.
       */
      title: string;
      type: "AcpToolCall";
    }
  | {
      type: "RecordMemory";
    }
  | {
      type: "SearchRegistry";
    }
  | {
      /**
       * The document the tool call targets; empty for `docs.create` (the document does not exist yet) and `docs.read` listings.
       */
      document_id: string;
      /**
       * A short human description of the access (e.g. `docs.edit block p`), for the trace.
       */
      summary: string;
      type: "DocumentEdit";
    }
  | {
      type: "WorkflowQuery";
      /**
       * The workflow run being read, or empty when listing the repository's runs (server-derived from the run context / validated args).
       */
      workflow_run_id: string;
    }
  | {
      summary: string;
      type: "WorkflowCreate";
      workflow_id: string;
    }
  | {
      /**
       * `named` or `inline`, so approval surfaces distinguish persistence from an ephemeral manifest without decoding tool arguments.
       */
      kind: string;
      summary: string;
      type: "WorkflowRun";
      workflow_id: string;
    }
  | {
      /**
       * The canonical repository whose board is written (server-derived from the run context, never model-supplied).
       */
      repository: string;
      /**
       * A short human rendering of the write (e.g. `create "wire the DAG"`).
       */
      summary: string;
      type: "TaskWrite";
    }
  | {
      /**
       * The canonical repository whose board is read (server-derived).
       */
      repository: string;
      type: "TaskRead";
    }
  | {
      name: string;
      summary: string;
      type: "CouncilCreate";
    }
  | {
      name: string;
      summary: string;
      type: "CouncilRun";
    }
  | {
      selector: string;
      type: "CouncilResultRead";
    }
  | {
      /**
       * The canonical repository whose graph is read (server-derived from the run context, never model-supplied).
       */
      repository: string;
      /**
       * A short human rendering of the question (e.g. `callers of Router::decide`), for the trace.
       */
      summary: string;
      type: "CodeGraphQuery";
    }
  | {
      /**
       * The canonical repository whose graph is written (server-derived from the run context, never model-supplied).
       */
      repository: string;
      /**
       * A short human rendering of the assertion (e.g. `assert handle_charge calls ChargeService::run`), for the trace.
       */
      summary: string;
      type: "CodeGraphAssert";
    }
  | {
      /**
       * The bounded `header` of each question, for the trace.
       */
      headers: string[];
      /**
       * How many questions the call carries.
       */
      question_count: number;
      type: "AskUser";
    }
  | {
      /**
       * The checkpoint commit being restored to.
       */
      commit: string;
      /**
       * The checkpoint's turn ordinal within the run.
       */
      ordinal: number;
      /**
       * The run whose worktree is rewound (string form of the RunId).
       */
      run_id: string;
      type: "RestoreCheckpoint";
      /**
       * The worktree directory the reset/clean/apply will run in.
       */
      worktree: string;
    }
  | {
      /**
       * The number of stdin bytes written (the payload itself is never carried, so no echoed secret reaches the ledger).
       */
      byte_len: number;
      /**
       * The id of the already-running process whose stdin is written (server-tracked; the model names an existing process, never spawns one here).
       */
      process_id: number;
      type: "WriteProcessStdin";
    }
  | {
      /**
       * The mode the accepted continuation runs in (`Plan` from `plan_enter`, `Build` from `plan_exit`).
       */
      target: AgentMode;
      type: "PlanTransition";
    }
  | {
      name: string;
      type: "ReadSecret";
    }
  | {
      type: "Unknown";
    };
/**
 * The outcome of a completed tool call, carried by `ToolCompleted`.
 *
 * Chapter 03 lists tool-completed as an event category without fixing its payload; this is the minimal reasonable shape — success, or failure with a short message. Bulk output travels as an `ArtifactRef`, never here.
 */
type ToolOutcome =
  | {
      type: "Succeeded";
    }
  | {
      message: string;
      type: "Failed";
    }
  | {
      type: "Unknown";
    };
/**
 * Severity buckets for a [`Risk`].
 */
type RiskLevel =
  | {
      type: "Low";
    }
  | {
      type: "Medium";
    }
  | {
      type: "High";
    }
  | {
      type: "Critical";
    }
  | {
      type: "Unknown";
    };
/**
 * Which budget a `BudgetWarning` is about. The unit of the reported `used`/`limit` is implied by the dimension (tokens, minor currency units, seconds, or a count of calls).
 */
type BudgetDimension =
  | {
      type: "Tokens";
    }
  | {
      type: "Cost";
    }
  | {
      type: "WallClock";
    }
  | {
      type: "ToolCalls";
    }
  | {
      type: "Unknown";
    };
/**
 * The terminal outcome of a run, carried by `RunCompleted`.
 *
 * Chapter 04 names the terminal `RunState`s but leaves the disposition detail open at Phase 1; this is the minimal reasonable shape — the terminal kind plus a short human-readable summary or reason.
 */
type RunDisposition =
  | {
      summary?: string | null;
      type: "Completed";
    }
  | {
      /**
       * The structured half of the failure: a stable code, whether a retry could succeed, and the affordance a client should offer — derived from the typed error where one exists, never from `reason`'s text. Absent from older daemons and from failures nothing has classified; a client reads it first and falls back to `reason`.
       */
      error?: CodypendentError | null;
      reason: string;
      type: "Failed";
    }
  | {
      reason?: string | null;
      type: "Cancelled";
    }
  | {
      type: "Unknown";
    };
/**
 * How a filesystem checkpoint is materialized (Adoption 04).
 */
type CheckpointKind =
  | {
      type: "Stash";
    }
  | {
      type: "Commit";
    }
  | {
      type: "Unknown";
    };
/**
 * A typed navigation target. Clients never need to interpret an arbitrary URL.
 */
type InboxDeepLink =
  | {
      approval_id: string;
      type: "Approval";
    }
  | {
      question_id: string;
      type: "Question";
    }
  | {
      session_id: string;
      type: "Session";
    }
  | {
      run_id: string;
      session_id: string;
      type: "Run";
    }
  | {
      type: "Workflow";
      workflow_id: string;
    }
  | {
      plugin_id: string;
      type: "Plugin";
    }
  | {
      repository_id: string;
      type: "Repository";
    }
  | {
      type: "Unknown";
    };
/**
 * Durable source identity from which the daemon derives the deduplication key.
 */
type InboxSourceIdentity =
  | {
      approval_id: string;
      type: "Approval";
    }
  | {
      question_id: string;
      type: "Question";
    }
  | {
      run_id: string;
      type: "Run";
    }
  | {
      budget_id: string;
      type: "Budget";
    }
  | {
      type: "Workflow";
      workflow_id: string;
    }
  | {
      plugin_id: string;
      type: "Plugin";
    }
  | {
      runner_id: string;
      type: "Runner";
    }
  | {
      type: "Unknown";
    };
/**
 * Semantic role of an archive entry.
 */
type BundleEntryKind =
  | {
      type: "TranscriptEvents";
    }
  | {
      type: "RoutingMetadata";
    }
  | {
      type: "Approvals";
    }
  | {
      type: "ArtifactManifest";
    }
  | {
      type: "Patch";
    }
  | {
      type: "EnvironmentDiagnostics";
    }
  | {
      type: "Unknown";
    };
/**
 * Kind of durable identity rewritten by an import.
 */
type BundleIdentityKind =
  | {
      type: "Session";
    }
  | {
      type: "Run";
    }
  | {
      type: "Artifact";
    }
  | {
      type: "Approval";
    }
  | {
      type: "ChangeSet";
    }
  | {
      type: "Unknown";
    };
/**
 * The content behind one of a memory's evidence refs — Chapter 06's "every retrieved memory opens its source", fetched rather than merely named.
 */
type MemoryEvidence =
  | {
      events: SessionEvent[];
      type: "Events";
    }
  | {
      bytes_base64: string;
      media_type: string;
      type: "Artifact";
    }
  | {
      type: "Unknown";
    };
/**
 * The lifecycle state of one workflow node, projected for a client. Mirrors `codypendent_workflow`'s `NodeState` across the wire; a value from a newer peer deserializes to [`Unknown`](WorkflowNodeState::Unknown) rather than failing the frame.
 */
type WorkflowNodeState =
  | {
      type: "Pending";
    }
  | {
      type: "Running";
    }
  | {
      type: "WaitingApproval";
    }
  | {
      type: "Blocked";
    }
  | {
      type: "Completed";
    }
  | {
      type: "Failed";
    }
  | {
      type: "Skipped";
    }
  | {
      type: "Unknown";
    };
/**
 * The lifecycle state of a workflow **run**, projected for a client. Mirrors `codypendent_workflow`'s `WorkflowRunState`; distinct from the agent-run [`RunState`](crate::run::RunState), which describes a single agent run rather than a durable multi-node workflow.
 */
type WorkflowRunPhase =
  | {
      type: "Pending";
    }
  | {
      type: "Running";
    }
  | {
      type: "Paused";
    }
  | {
      type: "Completed";
    }
  | {
      type: "Failed";
    }
  | {
      type: "Cancelled";
    }
  | {
      type: "Unknown";
    };
/**
 * One live event on a workflow run's observability stream ([`Subscription::Workflow`](crate::handshake::Subscription::Workflow)), delivered as [`Payload::WorkflowEvent`](crate::envelope::Payload::WorkflowEvent).
 *
 * A newer peer's variant deserializes to [`Unknown`](WorkflowEvent::Unknown) so an additive event never breaks an older client.
 */
type WorkflowEvent =
  | {
      /**
       * The 1-based attempt number (0 before the node has ever run).
       */
      attempt: number;
      cost?: JsonValue;
      /**
       * The node ids this node depends on — the graph's **edges**, so a client can draw the DAG rather than a flat list (rubric 5). Carried on snapshot reads (the daemon recompiles the run's stored manifest to recover them); a live [`WorkflowEvent::NodeTransitioned`] omits them (the graph shape is static per run), so a client merging live deliveries must preserve the edges it learned from the snapshot rather than overwriting them with an empty list. Additive (`#[serde(default)]`): an older daemon sends none and the field parses back empty.
       */
      depends_on?: string[];
      /**
       * The node's latest failure or budget-block reason, when its latest state is `Failed`/`Blocked` (a `Completed` transition clears it). `None` otherwise.
       */
      error?: string | null;
      /**
       * The node (step) id, unique within its workflow.
       */
      node_id: string;
      /**
       * The node's lifecycle state.
       */
      state: WorkflowNodeState;
      type: "NodeTransitioned";
      /**
       * Budget-dimension warnings raised while charging this node (each crossed 80% of a limit but stayed within it), pre-rendered. Empty when none.
       */
      warnings?: string[];
      /**
       * The workflow run this node belongs to.
       */
      workflow_run_id: string;
    }
  | {
      phase: WorkflowRunPhase;
      type: "RunPhaseChanged";
      workflow_run_id: string;
    }
  | {
      type: "Unknown";
    };
type Byte = number;
/**
 * What the daemon can say about a model without running it.
 *
 * `#[serde(other)] Unknown` so a client built against an older protocol folds a verdict it has never heard of into something it can still render, rather than failing to parse the whole reply.
 */
type ModelReadiness =
  | {
      detail: string;
      state: "ready";
    }
  | {
      detail: string;
      state: "unverified";
    }
  | {
      detail: string;
      /**
       * The classified cause, when the daemon has one: a stable code, retryability and the `user_action` a client turns into an affordance. Absent for a verdict with no typed cause behind it.
       */
      error?: CodypendentError | null;
      state: "unavailable";
    }
  | {
      state: "unknown";
    };
/**
 * The daemon's answer to an attach: replay or snapshot.
 */
type Catchup =
  | {
      events: SessionEvent[];
      from: number;
      through: number;
      type: "Events";
    }
  | {
      projection: SessionProjection;
      through: number;
      type: "Snapshot";
    }
  | {
      type: "Unknown";
    };
type UiPrimitivesSchema = UiWildcardSchema | string[];
type UiWildcardSchema = "*";
type UiPatch =
  | {
      node: UiNode;
      op: "replaceRoot";
    }
  | {
      index: number;
      node: UiNode;
      op: "insert";
      parentId: string;
    }
  | {
      nodeId: string;
      op: "remove";
    }
  | {
      node: UiNode;
      nodeId: string;
      op: "replace";
    }
  | {
      nodeId: string;
      op: "updateProps";
      set?: {
        [k: string]: JsonValue | undefined;
      };
      unset?: string[];
    }
  | {
      nodeId: string;
      op: "setText";
      text: string;
    }
  | {
      index: number;
      nodeId: string;
      op: "move";
      parentId: string;
    };
/**
 * State of a repository within a campaign.
 */
type CampaignRepoState = "pending" | "running" | "succeeded" | "failed" | "denied" | "skipped" | "unknown";

interface KnowledgePage {
  documents?: DocCard[];
  learnings?: LearningCard[];
  memories?: MemoryCard[];
  next_offset?: number | null;
  repository?: string | null;
  section: KnowledgeSection;
  skills?: SkillCard[];
  warnings?: string[];
  workspace_id?: string | null;
}
/**
 * `crates/tui/src/state.rs` `DocCard`.
 */
interface DocCard {
  blocks: DocBlockView[];
  document_id: string;
  mode: string;
  /**
   * Pre-rendered, e.g. `"r7"`.
   */
  revision: string;
  scope: string;
  status: string;
  suggestions: DocSuggestionView[];
  title: string;
}
/**
 * `crates/tui/src/state.rs` `DocBlockView`.
 */
interface DocBlockView {
  /**
   * The block's primary text verbatim, or `None` for a structured block with no single editable container.
   */
  editable?: string | null;
  id: string;
  kind: string;
  /**
   * A one-line, lossy display rendering.
   */
  text: string;
}
/**
 * `crates/tui/src/state.rs` `DocSuggestionView`.
 */
interface DocSuggestionView {
  author: string;
  block_id: string;
  id: string;
  original: string;
  range: string;
  rationale?: string | null;
  replacement: string;
  source_revision: number;
  status: string;
}
/**
 * `crates/tui/src/state.rs` `LearningCard`.
 */
interface LearningCard {
  confidence: number;
  id: string;
  kind: string;
  pinned: boolean;
  provenance: string;
  revision: number;
  scope: string;
  state: string;
  statement: string;
}
/**
 * `crates/tui/src/state.rs` `MemoryCard`, plus the memory's id so the desktop can address `CorrectMemory`/`ForgetMemory` at it.
 */
interface MemoryCard {
  class: string;
  confidence: number;
  id: string;
  /**
   * A date string, exactly as the store rendered it.
   */
  observed: string;
  revision: string;
  scope: string;
  /**
   * The human-readable evidence source; "(no evidence)" when there is none.
   */
  source: string;
  statement: string;
}
/**
 * `crates/tui/src/state.rs` `SkillCard`. `permissions` are verbatim strings.
 */
interface SkillCard {
  description: string;
  kind: string;
  name: string;
  permissions: string[];
  risk: string;
  scope: string;
  status: string;
  trust: string;
}
interface ProtocolVersion {
  major: number;
  minor: number;
}
/**
 * What a connected client can render and accept.
 *
 * All flags default to `false`: a client only gets richer projections after it explicitly opts in, so an unknown or minimal client is always served the safe, plain-text baseline.
 */
interface ClientCapabilities {
  /**
   * Can query measured usage and consume bounded exports.
   */
  analytics?: boolean;
  /**
   * Can capture microphone audio.
   */
  audio_capture?: boolean;
  /**
   * Can manage trigger and schedule automation bindings.
   */
  automation?: boolean;
  /**
   * Can export and import versioned redacted bundles.
   */
  bundles?: boolean;
  /**
   * Can render a side-by-side or unified diff.
   */
  diff_view?: boolean;
  /**
   * Can invoke ordinary runs from editor-native actions.
   */
  editor_actions?: boolean;
  /**
   * Owns an editor buffer the daemon may mutate semantically.
   */
  editor_mutations?: boolean;
  /**
   * Can display raster/vector images inline.
   */
  image_display?: boolean;
  /**
   * Can render and mutate the durable owner-scoped inbox.
   */
  inbox?: boolean;
  /**
   * Can manage marketplace packages and publisher trust.
   */
  marketplace?: boolean;
  /**
   * Reports mouse input (every mouse affordance also has a keyboard path).
   */
  mouse?: boolean;
  /**
   * Renders styled text (bold, colour spans, links).
   */
  rich_text?: boolean;
  /**
   * Can manage brokered secret references.
   */
  secrets?: boolean;
  /**
   * Can browse and navigate the cursor-paged Session Library.
   */
  session_library?: boolean;
  /**
   * Terminal/display supports 24-bit colour.
   */
  true_color?: boolean;
  /**
   * Terminal/display handles Unicode beyond ASCII.
   */
  unicode?: boolean;
}
/**
 * Request for ranked session search.
 */
interface SessionSearchQuery {
  cursor?: string | null;
  filters?: SessionSearchFilters;
  limit?: number;
  query: string;
}
/**
 * Filters applied together by the ranked session search service.
 */
interface SessionSearchFilters {
  created_after?: string | null;
  created_before?: string | null;
  model_ids?: string[];
  repository_ids?: string[];
  run_states?: RunState[];
  workflow_ids?: string[];
}
/**
 * Controls bounded data included in a session export.
 */
interface SessionExportOptions {
  format: SessionExportFormat;
  include_artifacts?: boolean;
  include_internal_sessions?: boolean;
}
/**
 * One editor diagnostic, forwarded from the IDE for context.
 */
interface Diagnostic {
  message: string;
  path: string;
  range: Range;
  severity: DiagnosticSeverity;
  source?: string | null;
}
/**
 * A half-open range within a single document.
 */
interface Range {
  end: Position;
  start: Position;
}
/**
 * A zero-based position in a text document.
 */
interface Position {
  character: number;
  line: number;
}
/**
 * Current editor state attached to an editor-native action.
 */
interface EditorActionContext {
  diagnostics?: Diagnostic[] | null;
  ide: IdeContextUpdate;
  repository_id?: string | null;
}
/**
 * A debounced snapshot of the IDE's context, pushed client→daemon. Clients debounce these (≥ 300 ms) so a burst of keystrokes collapses to one update.
 */
interface IdeContextUpdate {
  /**
   * The file the user is focused on, if any.
   */
  active_file?: string | null;
  /**
   * A monotonically increasing revision for the diagnostics set, so the daemon can tell whether it holds the latest without transferring them.
   */
  diagnostics_revision?: number;
  /**
   * Digests of every unsaved buffer (contents are never sent unsolicited).
   */
  dirty_buffers?: DirtyBufferDigest[];
  /**
   * Paths of all open documents.
   */
  open_files?: string[];
  /**
   * The current selection, if any.
   */
  selection?: EditorSelection | null;
}
/**
 * A content digest for one unsaved ("dirty") editor buffer. The filesystem is not always the user's current truth; the IDE sends digests so the daemon can detect divergence and request the full contents only when required and authorized (Chapter 10, "Unsaved buffers").
 */
interface DirtyBufferDigest {
  byte_length: number;
  path: string;
  /**
   * Lowercase hex SHA-256 of the buffer's current bytes.
   */
  sha256: string;
}
/**
 * The editor's current selection: a range within one file.
 */
interface EditorSelection {
  path: string;
  range: Range;
}
/**
 * Cursor-based inbox list request.
 */
interface InboxListQuery {
  cursor?: string | null;
  filters?: InboxListFilters;
  limit?: number | null;
}
/**
 * Optional list restrictions. An empty value means all authorized entries.
 */
interface InboxListFilters {
  kinds?: InboxEntryKind[];
  repository_ids?: string[];
  states?: InboxEntryState[];
}
/**
 * A bounded, cursor-paged aggregate query.
 */
interface AnalyticsQuery {
  cursor?: string | null;
  filters?: AnalyticsFilters;
  group_by?: AnalyticsGrouping[];
  /**
   * Requested page size. The server applies its own upper bound; zero means the server default.
   */
  limit?: number;
}
/**
 * Optional restrictions on observations. Empty lists do not restrict.
 */
interface AnalyticsFilters {
  completions?: AnalyticsCompletion[];
  models?: string[];
  providers?: string[];
  repositories?: string[];
  routes?: string[];
  task_classes?: string[];
  time?: AnalyticsTimeRange | null;
  workflows?: string[];
}
/**
 * Inclusive start and exclusive end of an analytics query.
 */
interface AnalyticsTimeRange {
  from?: string | null;
  until?: string | null;
}
/**
 * Request for a server-bounded export of an analytics query.
 */
interface AnalyticsExportRequest {
  format: AnalyticsExportFormat;
  /**
   * Requested row ceiling. The server may impose a smaller ceiling; zero selects the server default.
   */
  max_rows?: number;
  query: AnalyticsQuery;
}
interface AutomationBindingDraft {
  enabled?: boolean;
  filters?: TriggerFilters;
  invocation?: InvocationPolicy;
  name: string;
  repository_id: string;
  source: TriggerSource;
  workflow_id: string;
  workflow_version: string;
}
/**
 * Common source filters. Values are public event metadata, never credentials.
 */
interface TriggerFilters {
  actors?: string[];
  branches?: string[];
  labels?: string[];
  metadata?: {
    [k: string]: string | undefined;
  };
  paths?: string[];
}
/**
 * Per-binding invocation controls, independent of the workflow definition.
 */
interface InvocationPolicy {
  approval_mode?: AutomationApprovalMode & string;
  budget_ceiling?: BudgetCeiling | null;
  concurrency?: ConcurrencyPolicy & string;
  deduplication?: DeduplicationPolicy;
  missed_run?: MissedRunPolicy & string;
  retry?: TriggerRetryPolicy;
}
interface BudgetCeiling {
  cost_micros?: number | null;
  tokens?: number | null;
  tool_calls?: number | null;
  wall_time_seconds?: number | null;
}
interface DeduplicationPolicy {
  /**
   * Names of normalized event fields which form the identity.
   */
  identity_fields?: string[];
  window_seconds?: number;
}
interface TriggerRetryPolicy {
  backoff_multiplier?: number;
  initial_delay_seconds?: number;
  max_attempts?: number;
  max_delay_seconds?: number | null;
}
interface AutomationBindingQuery {
  cursor?: string | null;
  enabled?: boolean | null;
  limit?: number | null;
  repository_id?: string | null;
  workflow_id?: string | null;
}
/**
 * Sparse update. Nested policy values are replaced as a normalized unit.
 */
interface AutomationBindingPatch {
  enabled?: boolean | null;
  filters?: TriggerFilters | null;
  invocation?: InvocationPolicy | null;
  name?: string | null;
  repository_id?: string | null;
  source?: TriggerSource | null;
  workflow_id?: string | null;
  workflow_version?: string | null;
}
/**
 * A budget as a client asks for it. The owner is never on the wire: it is the connection's kernel-derived principal, exactly like every other owner-scoped store.
 */
interface AnalyticsBudgetDraft {
  dimension: AnalyticsBudgetDimension;
  enabled?: boolean;
  scope: AnalyticsBudgetScope;
  /**
   * Strictly positive (0043 `CHECK (threshold > 0)`): a zero threshold would alert on the first measured observation forever.
   */
  threshold: number;
  window: AnalyticsBudgetWindow;
}
/**
 * Optional narrowing of a budget listing. There is no cursor and no total: budgets are bounded per owner by 0043's UNIQUE constraint, and a count is the kind of aggregate that leaks volume, so the server caps the page and says so with `truncated` instead.
 */
interface AnalyticsBudgetQuery {
  enabled?: boolean | null;
  /**
   * Requested row ceiling; zero selects the server default. The server may impose a smaller ceiling.
   */
  limit?: number;
}
/**
 * A sparse update. An absent field is unchanged; scope, dimension and window are immutable because they are the row's UNIQUE identity — changing one is a delete plus a create, and pretending otherwise would silently collide.
 */
interface AnalyticsBudgetPatch {
  enabled?: boolean | null;
  threshold?: number | null;
}
/**
 * Request a deterministic bundle export.
 */
interface BundleExportRequest {
  inclusion: BundleInclusionPolicy;
  redaction_policy?: BundleRedactionPolicy;
  source_session_ids?: string[];
}
/**
 * Exact categories the caller permits an exporter to include.
 *
 * Every switch defaults to `false`; omission therefore cannot accidentally broaden an export when a newer exporter adds another category.
 */
interface BundleInclusionPolicy {
  approvals?: boolean;
  artifact_manifests?: boolean;
  environment_diagnostics?: boolean;
  patches?: boolean;
  routing_metadata?: boolean;
  transcript_events?: boolean;
}
/**
 * Request an import from a previously uploaded bundle artifact.
 */
interface BundleImportRequest {
  bundle: ArtifactRef;
  collision_policy?: BundleCollisionPolicy;
}
/**
 * A pointer to a stored artifact plus the metadata needed to handle it safely.
 *
 * `id` and `sha256` are deliberately independent: identical bytes dedup to one blob (keyed by `sha256`) but every occurrence is its own `ArtifactRef` with its own id and `sensitivity` (Chapter 14 / STEP 1.4). Classification checks always read the ref in hand, never a row looked up by hash.
 */
interface ArtifactRef {
  byte_length: number;
  id: string;
  /**
   * IANA media type, e.g. `text/plain` or `application/json`.
   */
  media_type: string;
  sensitivity: DataClassification;
  /**
   * Lowercase hex SHA-256 of the blob's bytes (the content address).
   */
  sha256: string;
}
/**
 * A normalized unit of user input: where it came from, the typed blocks it carries, the scope it applies at, and any bulk attachments.
 */
interface InputEnvelope {
  /**
   * Bulk artifacts referenced by the blocks (or attached alongside them).
   */
  attachments?: ArtifactRef[];
  blocks: InputBlock[];
  scope: ScopeLevel;
  source: InputSource;
}
/**
 * A transcript of an [`AudioArtifact`], linked back to its source audio.
 */
interface Transcript {
  /**
   * Where the transcription ran (local vs. off-device).
   */
  mode: TranscriptionMode;
  /**
   * The transcription model, if a hosted/known one produced it.
   */
  model?: string | null;
  /**
   * Whether the user reviewed/edited the transcript before submission (Chapter 10: "transcript review before submission").
   */
  reviewed?: boolean;
  /**
   * The audio artifact this transcript was produced from — the link that keeps the original reachable from the interpretation.
   */
  source_audio: string;
  text: string;
}
/**
 * A model's textual observation about an image.
 */
interface ModelObservation {
  model?: string | null;
  text: string;
}
/**
 * A rectangular region of an image (a crop or a coordinate reference).
 */
interface ImageRegion {
  height: number;
  label?: string | null;
  width: number;
  x: number;
  y: number;
}
/**
 * A proposed replacement over `[range_start, range_end)` of a block's text.
 */
interface SuggestionInput {
  block_id: string;
  range_end: number;
  range_start: number;
  rationale?: string | null;
  replacement: string;
}
/**
 * A request to lease a block range for exclusive writing (Chapter 03 / STEP 4.3). One writer per block-range; readers are unlimited. Reuses the Phase-1 lease machinery in the daemon; this is only the wire request shape.
 */
interface DocumentEditLease {
  /**
   * The block the writer intends to edit; `None` leases the whole document structure (block insert/delete/reorder).
   */
  block_id?: string | null;
  document_id: string;
}
/**
 * A client-authored blackboard artifact before it is stored (Phase B kanban — the write half `PostBlackboardItem` carries). The author is **not** here: the daemon builds it server-side from the issuing connection, exactly as the workflow executor builds an agent's author — a client never supplies its own attribution.
 */
interface BlackboardItemDraft {
  /**
   * Who the card is assigned to, if anyone.
   */
  assignee?: string | null;
  /**
   * The author's confidence in `[0, 1]`, if given.
   */
  confidence?: number | null;
  /**
   * Evidence references grounding the artifact. Claim-like kinds require at least one (the store enforces it); a `task` needs none.
   */
  evidence?: JsonValue[];
  /**
   * The typed artifact kind (`task` for board cards; any store kind is legal).
   */
  kind: string;
  /**
   * The card's position within its column (lower sorts first). Appended to the end of the column when absent.
   */
  ordinal?: number | null;
  payload: JsonValue;
  /**
   * The board column (`todo` / `doing` / `review` / `done`, or a validated free string). Defaults server-side to `todo` for a `task`.
   */
  status?: string | null;
}
/**
 * The filter `graph show` applies. Every field narrows; absent fields do not.
 *
 * **Scoping is not one of these fields.** The repository is carried by the command, resolved by the daemon from a filesystem path with its own single source of truth, and applied to every branch of this query — including [`node_id`](Self::node_id). There is no way to spell "some other checkout" here.
 */
interface CodeGraphQuery {
  /**
   * An unfiltered visual overview follows one recorded connected neighbourhood, including endpoint metadata.
   */
  connected_view?: boolean;
  /**
   * Include the edges incident to the selected nodes — **every** node the filter selects, not only the ones this page of nodes happens to show. The two row kinds are paged independently (see [`limit`](Self::limit)), so an edge between two nodes that both fall past the node page is still returned and still counted.
   */
  include_edges?: boolean;
  /**
   * Return metadata for endpoints of the returned edges in `related_nodes`, separately from the matching node page and its total. At most twice the edge limit can be returned; other query filters still select the main page.
   */
  include_neighbors?: boolean;
  /**
   * Include the nodes themselves. Both false is treated as both true — a query that selects nothing is a client bug, not a legal request. Both flags default to `false`, so this is exactly what the default `{}` query asks for: everything.
   */
  include_nodes?: boolean;
  /**
   * Exact stored node-kind scalar (`function`, `type`, …).
   */
  kind?: string | null;
  /**
   * Exact stored language scalar (`rust`, `python`, …).
   */
  language?: string | null;
  /**
   * Maximum rows of each kind — nodes and edges get a page each, not one shared budget. 0 asks for the server default; the server clamps any request to its own ceiling.
   */
  limit?: number;
  /**
   * Case-insensitive substring of the qualified name.
   */
  name?: string | null;
  /**
   * Exactly one node, by its stored id.
   *
   * This is the direct-by-id path, and it carries the **same** repository gate the list path does. A node id belonging to another checkout is answered identically to an id that does not exist anywhere (`graph.node-not-found`), so naming an id can never confirm that it exists elsewhere. A filter that is enforced only where a list is built is not a filter; it is an enumeration oracle with extra steps.
   */
  node_id?: string | null;
  /**
   * Repo-relative path prefix (`crates/cli/`), matched against `code_nodes.source_path`.
   */
  path?: string | null;
}
/**
 * Client request to update publication policy for a repository.
 */
interface UpdatePublicationPolicyRequest {
  max_class?: PublicationClass | null;
  max_classification?: DataClassification | null;
  publish_evidence_artifacts?: boolean | null;
  publish_signature_hashes?: boolean | null;
  publish_source_paths?: boolean | null;
  publish_symbol_names?: boolean | null;
}
/**
 * Filtered query for shared nodes and edges.
 */
interface FederatedGraphQuery {
  class_ceiling?: PublicationClass | null;
  cursor?: string | null;
  kind?: string | null;
  language?: string | null;
  limit?: number | null;
  node_id?: string | null;
  repository_id?: string | null;
  symbol_name?: string | null;
}
/**
 * Query for cross-repository blast radius analysis.
 */
interface BlastRadiusQuery {
  cursor?: string | null;
  limit?: number | null;
  max_depth?: number | null;
  node_id?: string | null;
  package?: string | null;
  repository: string;
  symbol_name?: string | null;
}
/**
 * Query to plan a cross-repository API or schema migration.
 */
interface MigrationPlanQuery {
  kind: CampaignKind;
  source_repository: string;
  source_symbol: string;
  target_repositories?: string[];
  target_symbol?: string | null;
}
/**
 * Query to suggest reviewers based on graph topology and changed symbols/paths.
 */
interface ReviewerSuggestionQuery {
  changed_paths?: string[];
  changed_symbols?: string[];
  limit?: number | null;
  repository: string;
}
/**
 * Request to create a new coordinated campaign.
 */
interface CreateCampaignRequest {
  idempotency_key: string;
  kind: CampaignKind;
  repositories: CampaignRepoEnrollment[];
  title: string;
  workflow_id: string;
}
/**
 * Enrollment specification for a repository in a campaign.
 */
interface CampaignRepoEnrollment {
  approval_mode?: CampaignApprovalMode & string;
  budget_minor_units?: number | null;
  repository: string;
  worktree_path?: string | null;
}
/**
 * Request to execute or re-drive a campaign.
 */
interface ExecuteCampaignRequest {
  campaign_id: string;
  retry_failed_only?: boolean;
}
/**
 * A granted document lease — the daemon's reply to an accepted `AcquireDocumentLease` (STEP 4.3 client transport). The client holds the `lease_id` as the capability to renew (a re-acquire of the same range) and to `ReleaseDocumentLease` when it stops editing; `expires_at` is when the lease lapses if neither happens, so a crashed holder never blocks the range forever.
 */
interface DocumentLeaseGrant {
  /**
   * The leased block, or `None` for a whole-document (structural) lease.
   */
  block_id?: string | null;
  document_id: string;
  /**
   * When the lease lapses unless renewed.
   */
  expires_at: string;
  /**
   * The server-minted lease id, returned only to the acquirer.
   */
  lease_id: string;
}
/**
 * Summary shown by session pickers and the Session Library.
 *
 * The first six fields are the original v0.9 contract. Everything after them is additive so historical payloads remain valid.
 */
interface SessionSummary {
  archived_at?: string | null;
  created_at: string;
  internal?: boolean;
  last_activity_at?: string | null;
  last_run_id?: string | null;
  parent_run_id?: string | null;
  parent_session_id?: string | null;
  pinned?: boolean;
  repository?: string | null;
  repository_id?: string | null;
  run_state?: RunState | null;
  session_id: string;
  state: string;
  title: string;
  updated_at: string;
  workspace?: string | null;
  workspace_id?: string | null;
}
/**
 * Cursor-paged ranked search results.
 */
interface SessionSearchPage {
  items: SessionSearchResult[];
  next_cursor?: string | null;
}
/**
 * A ranked search hit with a durable identity and navigable target.
 */
interface SessionSearchResult {
  deep_link: SessionDeepLink;
  excerpt?: string | null;
  scope: SessionSearchScope;
  score: number;
  session: SessionSummary;
  source: SessionSearchSource;
  stable_identity: string;
}
/**
 * Cursor-paged durable session history.
 */
interface SessionHistoryPage {
  items: SessionEvent[];
  next_cursor?: string | null;
}
interface SessionEvent {
  actor: Actor;
  body: EventBody;
  causation_id?: string | null;
  correlation_id?: string | null;
  occurred_at: string;
  sequence: number;
}
/**
 * A structured risk assessment attached to a proposed action or approval request. Chapter 14 leaves the exact shape open at Phase 1; this is the minimal reasonable form — a severity level plus human-readable reasons.
 */
interface Risk {
  level: RiskLevel;
  reasons?: string[];
}
/**
 * A machine-readable, correlated error.
 *
 * `code` is a stable dotted identifier (for example `protocol.unsupported-payload` or `policy.write-denied`) that receivers branch on; `message` is for humans only.
 */
interface CodypendentError {
  /**
   * Stable machine-readable code. Never parse `message` to decide behaviour.
   */
  code: string;
  correlation_id: string;
  details?: JsonValue;
  /**
   * Human-readable explanation.
   */
  message: string;
  /**
   * Whether an identical retry could succeed.
   */
  retryable: boolean;
  /**
   * A suggested next step the client can surface as an affordance.
   */
  user_action?: UserAction | null;
}
/**
 * Completeness of the measured subtotal. Requests count calls to the model driver, including failed/cancelled calls; each measured dimension counts the calls that reported it. Retries without their own usage leave the subtotal partial even when the final attempt reports a measurement. Absent metadata on older events means coverage is unknown.
 */
interface UsageCoverage {
  completion_requests: number;
  cost_requests: number;
  prompt_requests: number;
  requests: number;
  unmeasured_retries?: number;
}
/**
 * One question as asked. `custom` is carried on the wire but deliberately NOT advertised in the tool schema — the model can never disable free-text answers (opencode's Prompt/Info split).
 */
interface QuestionPrompt {
  /**
   * Allow typing a custom answer (default true).
   */
  custom?: boolean;
  /**
   * Very short label (≤ 30 chars) shown as the card/tab title.
   */
  header: string;
  /**
   * Allow selecting more than one option.
   */
  multiple?: boolean;
  /**
   * Available choices (may be empty only when `custom` is true).
   */
  options: QuestionOption[];
  /**
   * The complete question.
   */
  question: string;
}
/**
 * One selectable choice.
 */
interface QuestionOption {
  /**
   * Explanation of the choice (may be empty).
   */
  description?: string;
  /**
   * Display text (1–5 words, concise).
   */
  label: string;
}
/**
 * One pending prompt, as carried on the `PendingPromptsChanged` snapshot.
 */
interface PendingPromptView {
  delivery: PromptDelivery;
  id: string;
  mode: AgentMode;
  text: string;
}
/**
 * A cursor page returned by an inbox list operation.
 */
interface InboxPage {
  items: InboxEntry[];
  next_cursor?: string | null;
}
/**
 * Repository-authorized client projection of an inbox row.
 *
 * There is intentionally no `owner_id`: authorization and owner scoping are repository concerns and cannot be selected or asserted by a client.
 */
interface InboxEntry {
  acknowledged_at?: string | null;
  created_at: string;
  deep_link: InboxDeepLink;
  dismissed_at?: string | null;
  id: string;
  kind: InboxEntryKind;
  repository_id: string;
  /**
   * Set only when the authoritative source operation resolves. Inbox acknowledgement and dismissal never decide an approval or question.
   */
  resolved_at?: string | null;
  source: InboxSource;
  state?: InboxEntryState;
  summary?: string;
  title: string;
}
/**
 * Stable provenance used by the repository to deduplicate an entry.
 */
interface InboxSource {
  /**
   * Stable within an owner. Replaying the same source must reuse this key.
   */
  dedup_key: string;
  identity: InboxSourceIdentity;
  run_id?: string | null;
  session_id?: string | null;
  workflow_id?: string | null;
}
interface AnalyticsPage {
  items: AnalyticsBucket[];
  next_cursor?: string | null;
}
/**
 * A result bucket. Dimension keys correspond in order to `query.group_by`.
 */
interface AnalyticsBucket {
  dimensions?: string[];
  metrics: AnalyticsMetrics;
}
/**
 * Aggregate values for a grouping bucket.
 */
interface AnalyticsMetrics {
  cached_tokens?: number | null;
  completion_count?: number | null;
  /**
   * Measured USD cost in millionths of a dollar.
   */
  cost_micros?: number | null;
  cost_per_successful_task_micros?: number | null;
  coverage?: AnalyticsDimensionCoverage;
  escalation_count?: number | null;
  grader_score_micros?: number | null;
  input_tokens?: number | null;
  latency_ms?: number | null;
  output_tokens?: number | null;
  reasoning_tokens?: number | null;
  retry_count?: number | null;
}
/**
 * Coverage is explicit per nullable metric, making partial aggregates visible.
 */
interface AnalyticsDimensionCoverage {
  cached_tokens: MeasurementCoverage;
  completion_count: MeasurementCoverage;
  cost: MeasurementCoverage;
  cost_per_successful_task: MeasurementCoverage;
  escalation_count: MeasurementCoverage;
  grader_score: MeasurementCoverage;
  input_tokens: MeasurementCoverage;
  latency: MeasurementCoverage;
  output_tokens: MeasurementCoverage;
  reasoning_tokens: MeasurementCoverage;
  retry_count: MeasurementCoverage;
}
/**
 * Number of observations for which a dimension was measured.
 */
interface MeasurementCoverage {
  measured: number;
  total: number;
}
/**
 * Metadata for a completed export. Bulk JSON/CSV bytes live in the artifact.
 */
interface AnalyticsExportResult {
  artifact: ArtifactRef;
  format: AnalyticsExportFormat;
  generated_at: string;
  row_count: number;
  truncated?: boolean;
}
interface AutomationBinding {
  created_at: string;
  enabled?: boolean;
  filters?: TriggerFilters;
  id: string;
  invocation?: InvocationPolicy;
  name: string;
  repository_id: string;
  source: TriggerSource;
  updated_at: string;
  workflow_id: string;
  workflow_version: string;
}
interface AutomationBindingPage {
  items: AutomationBinding[];
  next_cursor?: string | null;
}
/**
 * A stored budget with its server-assigned identity and timestamps.
 */
interface AnalyticsBudget {
  created_at: string;
  dimension: AnalyticsBudgetDimension;
  enabled?: boolean;
  /**
   * Opaque server-minted id. A plain `String` rather than a UUID newtype because 0043 declares `id TEXT PRIMARY KEY` with no format constraint and rows predating this command may carry any text.
   */
  id: string;
  scope: AnalyticsBudgetScope;
  /**
   * Strictly positive (0043 `CHECK (threshold > 0)`): a zero threshold would alert on the first measured observation forever.
   */
  threshold: number;
  updated_at: string;
  window: AnalyticsBudgetWindow;
}
/**
 * One page of budgets owned by the requesting principal.
 */
interface AnalyticsBudgetPage {
  items: AnalyticsBudget[];
  /**
   * The server's ceiling cut the listing short. Honest truncation rather than a total the caller could not otherwise see.
   */
  truncated?: boolean;
}
/**
 * Successful export. Archive bytes remain behind an artifact reference.
 */
interface BundleExportReceipt {
  bundle: ArtifactRef;
  manifest: BundleManifest;
}
/**
 * Self-describing manifest stored in every bundle.
 */
interface BundleManifest {
  created_at: string;
  entries?: BundleEntryManifest[];
  format_version: number;
  inclusion?: BundleInclusionPolicy;
  /**
   * Lowercase hexadecimal SHA-256 of the canonical entry manifest.
   */
  manifest_sha256: string;
  redaction_policy?: BundleRedactionPolicy;
  redaction_summary?: BundleRedactionSummary;
  source_session_ids?: string[];
}
/**
 * One regular-file entry in the archive.
 */
interface BundleEntryManifest {
  byte_length: number;
  classification: DataClassification;
  kind: BundleEntryKind;
  /**
   * IANA media type.
   */
  media_type: string;
  /**
   * Normalized relative archive path. Importers still validate this value.
   */
  path: string;
  /**
   * Lowercase hexadecimal SHA-256 of the uncompressed entry bytes.
   */
  sha256: string;
}
/**
 * Auditable aggregate of material removed or replaced during export.
 */
interface BundleRedactionSummary {
  artifact_bodies_omitted?: number;
  credentials_omitted?: number;
  diagnostics_fields_omitted?: number;
  entries_omitted?: number;
  values_replaced?: number;
}
/**
 * Successful import result. No approvals or credentials are restored.
 */
interface BundleImportReceipt {
  identity_mappings?: BundleIdentityMapping[];
  imported_session_ids?: string[];
  provenance: BundleImportProvenance;
  skipped_entries?: number;
}
/**
 * Mapping from an opaque source identity to its newly allocated local one.
 */
interface BundleIdentityMapping {
  kind: BundleIdentityKind;
  local_id: string;
  /**
   * Provenance attached to the corresponding imported durable record.
   */
  provenance: BundleImportProvenance;
  source_id: string;
}
/**
 * Provenance attached to every durable record created by an import.
 */
interface BundleImportProvenance {
  /**
   * Lowercase hexadecimal SHA-256 of the imported archive bytes.
   */
  bundle_sha256: string;
  imported_at: string;
  /**
   * Lowercase hexadecimal SHA-256 asserted by the verified manifest.
   */
  manifest_sha256: string;
  source_session_ids?: string[];
}
/**
 * Wire match item for workspace file fuzzy search (Adoption 11 M2).
 */
export interface FileMatchWire {
  indices: number[];
  path: string;
  score: number;
}
/**
 * Durable Remote UI plugin lifecycle status returned by daemon management commands. Trust and execution authority remain daemon-owned; this is a display-only projection.
 */
export interface UiPluginLifecycleStatus {
  enabledScope?: string | null;
  id: string;
  state: string;
  updateApprovalReceipt?: string | null;
  updatePermissionDiff?: string | null;
  version: string;
}
/**
 * One memory as a client sees it.
 */
interface MemoryView {
  /**
   * The memory class as its stored wire name (`semantic`, `episodic`, …).
   */
  class: string;
  confidence: number;
  /**
   * One human-legible label per evidence ref, in the SAME order [`OpenMemoryEvidence`](crate::command::CommandBody::OpenMemoryEvidence)'s `evidence_index` addresses them — so "show me where this came from" is a position in this list, and a client never has to reconstruct an `EvidenceRef` it cannot type.
   */
  evidence?: string[];
  id: string;
  observed_at: string;
  scope: MemoryScope;
  sensitivity: DataClassification;
  statement: string;
  structured_value?: JsonValue;
  /**
   * The memories this one replaced, when it is itself a correction.
   */
  supersedes?: string[];
}
/**
 * A memory's scope as the two scalars the store indexes (`scope_tier` / `scope_key`). `key` is absent for the keyless `system` tier.
 */
interface MemoryScope {
  key?: string | null;
  tier: string;
}
/**
 * One stored blackboard artifact, projected for a client.
 *
 * A read-command reply carries a `Vec` of these (the run's board, kind-filtered); a subscription delivers one as each post/supersede lands. The `workflow_run_id` travels with the item so a client routes a live [`Payload::BlackboardPosted`](crate::envelope::Payload::BlackboardPosted) to the right board without consulting the enclosing frame.
 */
interface BlackboardItemView {
  /**
   * Who the card is assigned to, if anyone.
   */
  assignee?: string | null;
  author: JsonValue;
  /**
   * The repository this item's board serves, when the item lives on a repository task board rather than a real workflow run (its `workflow_run_id` is then the synthetic [`board_scope_id`]). Additive: an older daemon sends none and every field below parses back defaulted.
   */
  board_scope?: string | null;
  /**
   * The author's self-reported confidence in `[0, 1]`, if given.
   */
  confidence?: number | null;
  /**
   * Evidence references grounding the artifact (opaque JSON). Claim-like kinds require at least one; the store enforces it.
   */
  evidence?: JsonValue[];
  /**
   * The artifact's stable id (a UUIDv7 string).
   */
  id: string;
  /**
   * The typed artifact kind (`finding`, `decision`, `hypothesis`, …), as the manifest-facing string the `BlackboardStore` records.
   */
  kind: string;
  /**
   * The card's position within its column (lower sorts first).
   */
  ordinal?: number | null;
  payload: JsonValue;
  /**
   * The artifact's revision within its supersession chain (1 for an original).
   */
  revision: number;
  /**
   * The board column (`todo` / `doing` / `review` / `done`, or a validated free string), when this item is a board card.
   */
  status?: string | null;
  /**
   * The id of the item that superseded this one, if any — a live item has `None`.
   */
  superseded_by?: string | null;
  /**
   * The workflow run whose board holds it.
   */
  workflow_run_id: string;
}
/**
 * A workflow run's full observable state — the catch-up baseline a mid-run subscriber reads (via [`ReadWorkflowRun`](crate::command::CommandBody::ReadWorkflowRun)) before folding the live [`WorkflowEvent`] stream on top. Reconstructed from the durable store, so a late subscriber after a daemon restart still gets a truthful baseline.
 */
interface WorkflowRunSnapshot {
  /**
   * Every node's full current view, in topological order.
   */
  nodes: WorkflowNodeView[];
  /**
   * The run's current lifecycle phase.
   */
  phase: WorkflowRunPhase;
  /**
   * The run this snapshot is of.
   */
  workflow_run_id: string;
}
/**
 * One workflow node's full current state, projected for a client.
 *
 * Carried identically in a [`WorkflowRunSnapshot`] and in each live [`WorkflowEvent::NodeTransitioned`], so a client applies either by overwrite-by-`node_id` — an overlap between the snapshot baseline and the live stream is a harmless idempotent re-write. The `workflow_run_id` travels with the view so a client routes a live delivery to the right run without consulting the enclosing frame (the frame is not session-scoped).
 */
interface WorkflowNodeView {
  /**
   * The 1-based attempt number (0 before the node has ever run).
   */
  attempt: number;
  cost?: JsonValue;
  /**
   * The node ids this node depends on — the graph's **edges**, so a client can draw the DAG rather than a flat list (rubric 5). Carried on snapshot reads (the daemon recompiles the run's stored manifest to recover them); a live [`WorkflowEvent::NodeTransitioned`] omits them (the graph shape is static per run), so a client merging live deliveries must preserve the edges it learned from the snapshot rather than overwriting them with an empty list. Additive (`#[serde(default)]`): an older daemon sends none and the field parses back empty.
   */
  depends_on?: string[];
  /**
   * The node's latest failure or budget-block reason, when its latest state is `Failed`/`Blocked` (a `Completed` transition clears it). `None` otherwise.
   */
  error?: string | null;
  /**
   * The node (step) id, unique within its workflow.
   */
  node_id: string;
  /**
   * The node's lifecycle state.
   */
  state: WorkflowNodeState;
  /**
   * Budget-dimension warnings raised while charging this node (each crossed 80% of a limit but stayed within it), pre-rendered. Empty when none.
   */
  warnings?: string[];
  /**
   * The workflow run this node belongs to.
   */
  workflow_run_id: string;
}
/**
 * What one on-demand `graph build` did — the report that makes an empty graph self-explanatory.
 *
 * Additive by default: a field a future daemon does not compute must be absent-or-zero in a way the client renders as "not measured" rather than as a fact. Today every field is measured.
 */
interface CodeGraphScanReport {
  /**
   * Per-language breakdown of what landed, most nodes first.
   */
  by_language: CodeGraphLanguageCount[];
  /**
   * The fold reached the cap, so the graph is a **truncation** of the repository rather than the repository.
   */
  cap_hit: boolean;
  /**
   * Rows in `code_edges` for this repository after the fold.
   */
  edges: number;
  /**
   * How long the fold took, so a slow scan is visible rather than inferred.
   */
  elapsed_ms: number;
  /**
   * The scan's own per-repository file cap.
   */
  file_cap: number;
  /**
   * Of those, the ones actually folded into the graph. `files_supported` minus this is the count that matched a grammar and still yielded nothing: unreadable, or a parse the extractor rejected.
   */
  files_folded: number;
  /**
   * Files the ignore rules excluded, or that vanished between the walk and the read.
   */
  files_ignored: number;
  /**
   * Of those, the ones whose extension maps to a grammar — the candidates.
   */
  files_supported: number;
  /**
   * Files no grammar covers. On a repository whose graph is empty, this number and [`not_folded`](Self::not_folded) **are** the explanation.
   */
  files_unsupported: number;
  /**
   * Every file the walk visited, before any filter. The denominator.
   */
  files_walked: number;
  /**
   * Every grammar this build has. Sent with the report because the useful answer to "why did nothing fold?" is the pair "these extensions were seen" and "these are the ones that would have worked".
   */
  grammars?: CodeGraphGrammar[];
  /**
   * Rows in `code_nodes` for this repository after the fold.
   */
  nodes: number;
  /**
   * The unsupported extensions, most files first. **Bounded** by the extractor, so its `files` may sum to less than [`files_unsupported`](Self::files_unsupported) — a tree with a very long tail of one-off extensions is counted in full but named in part.
   */
  not_folded?: CodeGraphSkippedExtension[];
  /**
   * The checkout the daemon resolved and folded — never the directory the client happened to be standing in.
   */
  repository_root: string;
  /**
   * The revision every node written by this scan was stamped with.
   */
  revision: string;
}
/**
 * One language's contribution to the graph, on the scan path and the stored path alike. `language` is the stored `code_nodes.language` scalar.
 */
interface CodeGraphLanguageCount {
  edges: number;
  /**
   * Distinct source files this language contributed.
   */
  files: number;
  language: string;
  nodes: number;
}
/**
 * One grammar this build carries, and the extensions that select it. Sent so a client can answer "what *would* have been folded?" without keeping its own copy of the roster.
 */
interface CodeGraphGrammar {
  /**
   * Lowercase, no leading dot.
   */
  extensions: string[];
  language: string;
}
/**
 * Files an extension contributed to the walk that no grammar covers. `extension` is lowercase and carries no leading dot; a file with no extension at all is not tallied here (it has nothing to tally under) but still counts in [`CodeGraphScanReport::files_unsupported`].
 */
interface CodeGraphSkippedExtension {
  extension: string;
  files: number;
}
/**
 * One configured model's readiness, as the daemon sees it.
 */
interface ModelProbe {
  /**
   * The `models.toml` id, not the provider's model name.
   */
  id: string;
  /**
   * Whether the network was actually used to reach this verdict.
   *
   * A client shows a credentials-only verdict differently from a probed one, because only the latter proves the provider lists the model.
   */
  probed: boolean;
  readiness: ModelReadiness;
}
/**
 * What the stored graph holds for one repository right now, with no re-scan.
 */
interface CodeGraphStatusView {
  /**
   * Node kinds (`function`, `type`, `file`, …), most nodes first.
   */
  by_kind: CodeGraphTally[];
  by_language: CodeGraphLanguageCount[];
  edges: number;
  /**
   * Distinct `source_path` values across the repository's nodes.
   */
  files: number;
  /**
   * The checkout's current `HEAD` (or `workdir` where Git cannot answer).
   */
  head_revision: string;
  nodes: number;
  repository_root: string;
  /**
   * The revisions the stored nodes are stamped at, most nodes first. More than one entry means the graph is a mix — usually a full scan at a commit plus incremental `<head>+workdir` folds on top.
   */
  revisions: CodeGraphTally[];
  /**
   * The graph does not describe the current working tree.
   */
  stale: boolean;
  /**
   * Why, in one sentence, when `stale`. Absent when the graph is current.
   */
  stale_reason?: string | null;
  /**
   * The working tree has uncommitted changes to tracked files.
   */
  working_tree_dirty: boolean;
}
/**
 * A labelled tally — a node kind, or a revision the graph was stamped at.
 */
interface CodeGraphTally {
  count: number;
  label: string;
}
/**
 * One page of `graph show` results.
 */
interface CodeGraphPage {
  edges: CodeGraphEdgeView[];
  /**
   * The limit actually applied after the server's clamp.
   */
  limit: number;
  nodes: CodeGraphNodeView[];
  /**
   * Optional endpoint metadata requested by `include_neighbors`. These are not additional filter matches and do not change `total_nodes`.
   */
  related_nodes?: CodeGraphNodeView[];
  /**
   * Edges incident to the filter's nodes, likewise **before** the limit and likewise over the whole filtered node set — a client renders it as the `M` in "showing N of M", so a total clamped to the page, or narrowed to the nodes one page showed, is a wrong number presented as a fact.
   */
  total_edges: number;
  /**
   * Nodes matching the filter **before** the limit, so a client can say "showing 50 of 812" rather than implying it showed everything.
   */
  total_nodes: number;
}
/**
 * One edge, projected for display with both endpoints already named (a client must not have to issue a second query to render an edge).
 */
interface CodeGraphEdgeView {
  /**
   * Present exactly when this edge's stored evidence is an agent assertion — i.e. for an `agent_asserted` edge written by `graph.assert_edge`. Absent for every mechanically-derived edge (whose evidence points at bytes, not a judgement), so the common case stays byte-identical on the wire.
   */
  asserted_by?: CodeGraphEdgeAssertion | null;
  /**
   * The recorded call occurrence, verified against its parsed source bytes. Repeated calls may share one stored edge; this is not an exhaustive list.
   */
  call_site?: CodeGraphSourceLocation | null;
  confidence: number;
  evidence_kind: string;
  from_id: string;
  from_name: string;
  relation: string;
  revision: string;
  to_id: string;
  to_name: string;
}
/**
 * Who asserted an `agent_asserted` edge, and why.
 *
 * The knowledge layer already records all three of these into `code_edges.evidence_artifact` (an `EvidenceRef::AgentAssertion`) on every `graph.assert_edge` call, and the tool *requires* the rationale. Without them on the wire a client can see only that some edge is agent-asserted — which is the claim, not the audit trail, and the audit trail is the reason a model is allowed to write to the graph at all. `evidence_kind` says an assertion happened; this says who made it and on what grounds, so a reviewer can go read the turn that said it.
 */
interface CodeGraphEdgeAssertion {
  /**
   * The reason the agent gave, verbatim. Free text, bounded by the tool at 400 characters — a renderer must wrap or truncate it rather than assume it fits a row.
   */
  rationale: string;
  /**
   * The run that made the claim.
   */
  run_id: string;
  /**
   * The session whose ledger holds the asserting turn.
   */
  session_id: string;
}
/**
 * A location in the parsed repository source. Lines and Unicode-character columns are one-based; the optional end position is exclusive.
 */
interface CodeGraphSourceLocation {
  column: number;
  end_column?: number | null;
  end_line?: number | null;
  line: number;
  source_path: string;
}
/**
 * One node, projected for display.
 */
interface CodeGraphNodeView {
  /**
   * The stored `code_nodes.id`. Naming it back in a [`CodeGraphQuery::node_id`] is scoped to the same repository — see that field's documentation.
   */
  id: string;
  kind: string;
  language: string;
  /**
   * Recorded definition span, present only when the source still matches the parser's evidence. Unresolved references have no definition location.
   */
  location?: CodeGraphSourceLocation | null;
  package?: string | null;
  qualified_name: string;
  revision: string;
  source_path?: string | null;
}
/**
 * A compact summary of session state sent in place of a long event history.
 *
 * Chapter 03 references a `SessionProjection` without fixing its fields; this is the minimal reasonable Phase 1 shape — enough for a reconnecting client to render a session's identity and live runs before it resumes live-tailing. Richer per-view projections arrive with their subscriptions.
 */
interface SessionProjection {
  active_runs?: string[];
  closed: boolean;
  /**
   * The highest event sequence folded into this snapshot.
   */
  last_sequence: number;
  /**
   * Approvals which are still actionable at the snapshot watermark. A compacted catch-up must preserve workflow state, not merely run ids.
   */
  pending_approvals?: PendingApprovalProjection[];
  /**
   * Pending queued prompts at the snapshot watermark, so a >500-event catch-up still shows the queue (mirrors `pending_approvals`).
   */
  pending_prompts?: PendingPromptView[];
  session_id: string;
  title: string;
}
/**
 * The actionable part of a pending approval carried in a compact snapshot.
 */
interface PendingApprovalProjection {
  action: ProposedAction;
  approval_id: string;
  pattern?: string | null;
  risk: Risk;
  run_id: string;
}
/**
 * A forward-compatible top-level message for dedicated remote UI transports. `kind` selects the populated optional payload. Unknown kinds remain deserializable and can carry `extensions` plus a fallback/error.
 */
interface UiWireMessage {
  action?: UiActionInvocation | null;
  actionResult?: UiActionResult | null;
  cancellation?: UiActionCancellation | null;
  capabilities?: UiCapabilities | null;
  contributions?: UiContributionRegistration[];
  dispose?: UiDispose | null;
  error?: UiRemoteError | null;
  event?: UiEvent | null;
  extensions?: {
    [k: string]: JsonValue | undefined;
  };
  hotReload?: UiHotReload | null;
  messageId: string;
  patchBatch?: UiPatchBatch | null;
  projection?: UiProjectionUpdate | null;
  resync?: UiResyncRequest | null;
  selection?: UiCapabilitySelection | null;
  snapshot?: UiSnapshot | null;
  subscription?: UiProjectionSubscription | null;
  theme?: UiTheme | null;
  type: string;
  unsubscription?: UiProjectionUnsubscription | null;
  viewport?: UiViewport | null;
}
/**
 * Validated command intent produced from an action binding. The host remains responsible for permission checks and rejects stale `revision` values.
 */
interface UiActionInvocation {
  actionId: string;
  documentId: string;
  formData?: {
    [k: string]: JsonValue | undefined;
  };
  /**
   * Event class to which the one-shot authority was bound.
   */
  interactionEventType?: string | null;
  /**
   * Echo of the broker-minted authority from the active event context.
   */
  interactionToken?: string | null;
  invocationId: string;
  payload?: JsonValue;
  revision: number;
  sourceNodeId: string;
}
/**
 * Resolution of a component-originated action/command invocation.
 */
interface UiActionResult {
  error?: UiRemoteError | null;
  invocationId: string;
  status: string;
  value?: JsonValue;
}
/**
 * Structured renderer/protocol error with a safe fallback and recovery hint.
 */
interface UiRemoteError {
  code: string;
  details?: JsonValue;
  documentId?: string | null;
  fallback?: UiFallback | null;
  message: string;
  nodeId?: string | null;
  patchIndex?: number | null;
  recoverable?: boolean;
  recovery?: string | null;
}
/**
 * Plain-text or simpler semantic fallback for unsupported client features.
 */
interface UiFallback {
  behavior?: string | null;
  plainText?: string | null;
  replacement?: UiNode | null;
}
interface UiNode {
  children?: UiNode[];
  fallback?: UiNode | null;
  id?: string | null;
  kind: string;
  props?: UiNodeProps;
  requires?: UiRequirement[];
  text?: string | null;
  type?: string | null;
}
/**
 * Typed common properties plus flattened extension properties. This serializes as the SDK's ordinary `props` JSON object while allowing Rust renderers to consume the stable semantic subset without stringly typed field access.
 */
interface UiNodeProps {
  accessibility?: UiAccessibility | null;
  attributes?: {
    [k: string]: JsonValue | undefined;
  };
  content?: UiContent | null;
  eventBindings?: UiActionBinding[];
  feedback?: UiFeedback | null;
  input?: UiInput | null;
  layout?: UiLayout | null;
  navigation?: UiNavigation | null;
  role?: string | null;
  structuredData?: UiData | null;
  style?: UiStyle | null;
  value?: JsonValue;
  [k: string]: unknown | undefined;
}
/**
 * Accessibility metadata required to give graphical and terminal clients the same semantic representation.
 */
interface UiAccessibility {
  description?: string | null;
  focusOrder?: number | null;
  headingLevel?: number | null;
  hidden?: boolean;
  keyboardHint?: string | null;
  label?: string | null;
  liveRegion?: string | null;
  role?: string | null;
  textFallback?: string | null;
}
/**
 * Shared content properties for Text, Markdown, Code, Diff, Image, Audio, JsonTree, LogViewer, and custom content primitives.
 */
interface UiContent {
  alternateText?: string | null;
  language?: string | null;
  lineWrap?: string | null;
  resource?: UiResourceReference | null;
  spans?: UiTextSpan[];
  text?: string | null;
}
/**
 * A resource is referenced, never embedded as unbounded bytes in the UI tree.
 */
interface UiResourceReference {
  byteLength?: number | null;
  digest?: string | null;
  mediaType: string;
  uri: string;
}
/**
 * One rich-text span within a content primitive.
 */
interface UiTextSpan {
  accessibilityLabel?: string | null;
  link?: string | null;
  style?: UiStyle | null;
  text: string;
}
/**
 * Semantic styling references theme tokens; producers never emit ANSI escapes, CSS, or raw terminal control sequences.
 */
interface UiStyle {
  background?: string | null;
  borderColor?: string | null;
  borderStyle?: string | null;
  emphasis?: string[];
  foreground?: string | null;
  opacity?: number | null;
  tone?: string | null;
  truncate?: string | null;
  visibility?: string | null;
}
/**
 * A semantic event-to-command binding. The host validates `action_id` and capabilities again when an invocation arrives; this declaration is never authority to perform I/O by itself.
 */
interface UiActionBinding {
  actionId: string;
  confirmation?: string | null;
  disabled?: boolean;
  event: string;
  payload?: JsonValue;
  requires?: string[];
}
/**
 * Progress, status, and tone shared by feedback primitives.
 */
interface UiFeedback {
  current?: number | null;
  indeterminate?: boolean | null;
  maximum?: number | null;
  message?: string | null;
  status?: string | null;
  tone?: string | null;
}
/**
 * Input state. Secret values must never be placed in a remote tree; secret entry is a host-owned contribution point.
 */
interface UiInput {
  defaultValue?: JsonValue;
  disabled?: boolean;
  inputType?: string | null;
  name?: string | null;
  options?: UiInputOption[];
  placeholder?: string | null;
  readOnly?: boolean;
  required?: boolean;
  validationMessage?: string | null;
  value?: JsonValue;
}
/**
 * One selectable value for input primitives.
 */
interface UiInputOption {
  description?: string | null;
  disabled?: boolean;
  id: string;
  label: string;
  value: JsonValue;
}
/**
 * Semantic layout hints. Hosts remain authoritative for clipping, cell width, responsive collapse, and terminal-safe placement.
 */
interface UiLayout {
  align?: string | null;
  basis?: UiDimension | null;
  columnGap?: number | null;
  columns?: UiDimension[];
  direction?: string | null;
  gap?: number | null;
  grow?: number | null;
  height?: UiDimension | null;
  justify?: string | null;
  margin?: UiEdges | null;
  maxHeight?: UiDimension | null;
  maxWidth?: UiDimension | null;
  minHeight?: UiDimension | null;
  minWidth?: UiDimension | null;
  overflow?: string | null;
  padding?: UiEdges | null;
  rowGap?: number | null;
  rows?: UiDimension[];
  shrink?: number | null;
  width?: UiDimension | null;
  wrap?: string | null;
}
/**
 * Renderer-neutral dimension. `unit` is normally `cells`, `percent`, `fr`, or `auto`, but is intentionally open-ended.
 */
interface UiDimension {
  unit: string;
  value: number;
}
/**
 * Four-sided spacing in terminal cells or renderer logical units.
 */
interface UiEdges {
  bottom?: number;
  left?: number;
  right?: number;
  top?: number;
}
/**
 * Navigation state shared by links, tabs, menus, trees, and command lists.
 */
interface UiNavigation {
  destination?: string | null;
  disabled?: boolean | null;
  expanded?: boolean | null;
  selected?: boolean | null;
  target?: string | null;
}
/**
 * Renderer-neutral structured data. Schemas and items remain JSON so a new chart, graph, or plugin-specific renderer does not require a protocol bump.
 */
interface UiData {
  columns?: UiDataColumn[];
  cursor?: string | null;
  items?: JsonValue[];
  kind?: string | null;
  schema?: JsonValue;
  selectedIds?: string[];
  total?: number | null;
}
/**
 * Column metadata used by tables and other structured-data primitives.
 */
interface UiDataColumn {
  id: string;
  label: string;
  sortable?: boolean;
  valueType?: string | null;
  width?: UiDimension | null;
}
/**
 * Presentation feature required by a node. Unknown features remain representable and are resolved through the node fallback.
 */
interface UiRequirement {
  feature: string;
  optional?: boolean;
}
/**
 * Idempotent cancellation of an in-flight mediated command invocation.
 */
interface UiActionCancellation {
  invocationId: string;
}
/**
 * Client or host capability advertisement. All named sets are open-ended and negotiated by intersection. A missing feature means the plain-text baseline.
 */
interface UiCapabilities {
  /**
   * Additive host command capabilities, independent of presentation.
   */
  capabilities?: string[];
  client: string;
  clipboard: boolean;
  colorDepth: string;
  /**
   * Additive plugin mount points understood by the host.
   */
  contributionPoints?: string[];
  daemon: ClientCapabilities;
  keyboard: boolean;
  limits?: UiHardLimits;
  media?: string[];
  primitives: UiPrimitivesSchema;
  protocolVersions: UiProtocolVersion[];
  reducedMotion: boolean;
  screenReader: boolean;
  terminalGraphics?: string[];
  viewport: UiViewport;
}
/**
 * Hard resource ceilings applied before a tree or patch reaches a renderer. Defaults are deliberately conservative enough for a full-screen app while bounding CPU, allocation, and pathological recursive/value input.
 */
interface UiHardLimits {
  maxActionsPerNode?: number;
  maxContributions?: number;
  maxJsonDepth?: number;
  maxJsonValues?: number;
  maxNodes?: number;
  maxPatchBytes?: number;
  maxPatchesPerBatch?: number;
  maxPropertiesPerNode?: number;
  maxTextBytes?: number;
  maxTreeDepth?: number;
}
/**
 * Version of the remote UI contract, negotiated independently of the daemon envelope protocol so renderers can evolve at their own pace.
 */
interface UiProtocolVersion {
  major: number;
  minor: number;
}
/**
 * Bounded viewport advertised by a rendering client.
 */
interface UiViewport {
  density?: number | null;
  height: number;
  pixelHeight?: number | null;
  pixelWidth?: number | null;
  width: number;
}
/**
 * Where and how one package-provided surface is mounted. `point` and `slot` are strings so new host surfaces do not require a protocol release.
 */
interface UiContributionRegistration {
  documentId: string;
  extensionId: string;
  id: string;
  metadata?: {
    [k: string]: JsonValue | undefined;
  };
  point: string;
  priority?: number;
  requires?: string[];
  slot: string;
  when?: string | null;
}
/**
 * Host instruction to unmount one document at its current revision.
 */
interface UiDispose {
  documentId: string;
  revision: number;
}
/**
 * A host-normalized event emitted by a semantic node.
 */
interface UiEvent {
  documentId: string;
  eventId: string;
  /**
   * Opaque one-shot host authority. Renderers never mint this value; the broker adds it only to the owner-bound event forwarded to a worker.
   */
  interactionToken?: string | null;
  modifiers?: UiEventModifiers | null;
  payload?: JsonValue;
  protocolVersion: UiProtocolVersion;
  revision: number;
  targetId: string;
  timestamp?: string | null;
  type: string;
}
/**
 * Keyboard/pointer modifiers accompanying a semantic event.
 */
interface UiEventModifiers {
  alt?: boolean;
  control?: boolean;
  meta?: boolean;
  shift?: boolean;
}
/**
 * Development-runtime notification that compiled modules changed.
 */
interface UiHotReload {
  changedModules: string[];
  generation: number;
}
/**
 * Atomic, ordered set of mutations from `base_revision` to the immediately following `revision`.
 */
interface UiPatchBatch {
  atomic?: boolean;
  baseRevision: number;
  documentId: string;
  fallback?: UiFallback | null;
  issuedAt?: string | null;
  patches: UiPatch[];
  protocolVersion: UiProtocolVersion;
  revision: number;
}
/**
 * Latest-wins data delivered for one mediated subscription. The worker never receives a raw path, database handle, socket, or secret through this value.
 */
interface UiProjectionUpdate {
  removed?: boolean;
  revision?: number | null;
  subscriptionId: string;
  value?: JsonValue;
}
/**
 * Runtime request for a fresh snapshot after a missing or rejected patch.
 */
interface UiResyncRequest {
  documentId: string;
  knownRevision?: number | null;
}
/**
 * Result of intersecting a producer/host offer with a rendering client.
 */
interface UiCapabilitySelection {
  capabilities: string[];
  colorDepth: number;
  contributionPoints: string[];
  imageProtocols: string[];
  limits: UiHardLimits;
  mouse: boolean;
  primitives: string[];
  protocolVersion: UiProtocolVersion;
  screenReader: boolean;
  unicode: boolean;
  viewport?: UiViewport | null;
}
/**
 * Full-state baseline used on mount, reconnect, patch rejection, and renderer recovery.
 */
interface UiSnapshot {
  document: UiDocument;
  reason?: string | null;
}
/**
 * A complete semantic UI tree at one revision.
 */
interface UiDocument {
  capabilities?: UiCapabilities | null;
  compatibility?: UiCompatibility | null;
  documentId: string;
  metadata?: {
    [k: string]: JsonValue | undefined;
  };
  protocolVersion: UiProtocolVersion;
  revision: number;
  root: UiNode;
}
/**
 * Compatibility requirements attached to a complete document.
 */
interface UiCompatibility {
  fallback?: UiFallback | null;
  minimumProtocol?: UiProtocolVersion | null;
  requiredCapabilities?: string[];
  requiredPrimitives?: string[];
}
/**
 * A component's mediated projection subscription. `kind` is open-ended (`session`, `run`, `artifact`, `command`, ...); the daemon authorizes each request against the plugin manifest before returning data.
 */
interface UiProjectionSubscription {
  kind: string;
  parameters?: {
    [k: string]: JsonValue | undefined;
  };
  resourceId?: string | null;
  subscriptionId: string;
}
/**
 * Semantic theme tokens. Token values are JSON scalars or small structured values so future renderers can add gradients or terminal-specific palettes without changing the contract.
 */
interface UiTheme {
  colorScheme?: string | null;
  highContrast?: boolean;
  id: string;
  name: string;
  reducedMotion?: boolean;
  revision: number;
  tokens?: {
    [k: string]: JsonValue | undefined;
  };
}
/**
 * Owner-scoped teardown for one mediated projection subscription.
 */
interface UiProjectionUnsubscription {
  subscriptionId: string;
}
/**
 * A public package/install metadata view returned by marketplace commands.
 */
interface MarketplacePackageView {
  displayName: string;
  enabledScope?: string | null;
  id: string;
  kind: string;
  latestVersion: string;
  lifecycle?: string | null;
  pinned?: boolean;
  pinnedVersion?: string | null;
  publisherId: string;
  summary: string;
}
/**
 * Opaque secret reference metadata returned to clients (never contains secret values).
 */
interface SecretReferenceView {
  backend: string;
  capability: string;
  createdAt: string;
  id: string;
  locator: string;
  name: string;
  organizationId?: string | null;
  ownerUid: number;
  repositoryId?: string | null;
  revokedAt?: string | null;
  revokedReason?: string | null;
  rotatedAt?: string | null;
}
/**
 * Durable federated identity view of a repository.
 */
interface FederatedRepositoryIdentityView {
  display_name: string;
  established_at: string;
  federated_id: string;
  normalized_remote?: string | null;
  repository_id: string;
  root_commit: string;
}
/**
 * Publication policy view for a repository.
 */
interface GraphPublicationPolicyView {
  max_class: PublicationClass;
  max_classification: DataClassification;
  policy_version: number;
  publish_evidence_artifacts: boolean;
  publish_signature_hashes: boolean;
  publish_source_paths: boolean;
  publish_symbol_names: boolean;
  repository_id: string;
  updated_at: string;
}
/**
 * Summary report of a publication batch.
 */
interface PublicationBatchSummary {
  acknowledged_at?: string | null;
  batch_hash?: string | null;
  batch_id: string;
  fact_count: number;
  policy_version: number;
  repository_id: string;
  sealed_at?: string | null;
  state: string;
}
/**
 * Paginated result of a federated graph query.
 */
interface FederatedGraphPage {
  cursor?: string | null;
  edges: SharedEdgeView[];
  has_more: boolean;
  nodes: SharedNodeView[];
}
/**
 * Published edge fact projection view.
 */
interface SharedEdgeView {
  class: PublicationClass;
  classification: DataClassification;
  confidence: number;
  evidence_artifact?: string | null;
  evidence_kind: string;
  from_repository_id: string;
  from_shared_node_id: string;
  relation: string;
  revision: string;
  shared_edge_id: string;
  to_repository_id: string;
  to_shared_node_id: string;
}
/**
 * Published node fact projection view.
 */
interface SharedNodeView {
  class: PublicationClass;
  classification: DataClassification;
  kind: string;
  language: string;
  package?: string | null;
  qualified_name?: string | null;
  repository_id: string;
  revision: string;
  shared_node_id: string;
  signature_hash?: string | null;
  source_path?: string | null;
}
/**
 * Result report of a blast radius query.
 */
interface BlastRadiusReport {
  affected_nodes: BlastRadiusNode[];
  affected_repositories: string[];
  cursor?: string | null;
  edge_count: number;
  has_more: boolean;
  seed_node_id: string;
}
/**
 * A node in a blast radius result graph.
 */
interface BlastRadiusNode {
  class: PublicationClass;
  depth: number;
  display_name: string;
  kind: string;
  relation_path?: string[];
  repository_id: string;
  shared_node_id: string;
}
/**
 * Report of a cross-repository migration plan.
 */
interface MigrationPlanReport {
  kind: CampaignKind;
  source_repository: string;
  steps: MigrationPlanStep[];
  title: string;
  total_affected_repositories: number;
}
/**
 * A step in an architectural migration plan.
 */
interface MigrationPlanStep {
  action: string;
  estimated_risk: string;
  repository_id: string;
  step_number: number;
  target_symbols: string[];
}
/**
 * Container for reviewer suggestions.
 */
interface ReviewerSuggestions {
  suggestions: ReviewerSuggestion[];
}
/**
 * A suggested reviewer with confidence and reasoning.
 */
interface ReviewerSuggestion {
  confidence: number;
  reason: string;
  relevant_repositories: string[];
  relevant_symbols: string[];
  reviewer_id: string;
}
/**
 * Full detail view of a campaign and all child projections.
 */
interface CampaignDetailView {
  approvals: CampaignApprovalView[];
  campaign: CampaignView;
  effects: CampaignEffectView[];
  repositories: CampaignRepositoryView[];
  runs: CampaignRunView[];
}
/**
 * View of an approval decision within a campaign repository slot.
 */
interface CampaignApprovalView {
  action_digest: string;
  approval_id: string;
  campaign_id: string;
  decided_at?: string | null;
  decision: string;
  repository_id: string;
}
/**
 * Summary view of a coordinated campaign.
 */
interface CampaignView {
  created_at: string;
  id: string;
  kind: CampaignKind;
  repository_count: number;
  state: CampaignState;
  terminal_at?: string | null;
  title: string;
  updated_at: string;
  workflow_id: string;
}
/**
 * View of an effect recorded in the campaign effect ledger.
 */
interface CampaignEffectView {
  applied_at: string;
  campaign_id: string;
  effect_digest: string;
  effect_kind: string;
  id: string;
  repository_id: string;
  run_id: string;
}
/**
 * View of an enrolled repository in a campaign.
 */
interface CampaignRepositoryView {
  approval_mode: CampaignApprovalMode;
  budget_minor_units?: number | null;
  campaign_id: string;
  enrolled_at: string;
  federated_id: string;
  repository_id: string;
  state: CampaignRepoState;
  terminal_at?: string | null;
  worktree_path?: string | null;
}
/**
 * View of a child workflow run under a campaign.
 */
interface CampaignRunView {
  attempt: number;
  campaign_id: string;
  created_at: string;
  repository_id: string;
  run_id: string;
  state: string;
  terminal_at?: string | null;
}

type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };
