import type { UiWireMessage } from "@codypendent/ui";
/**
 * The desktop client's only route to `codypendentd`.
 *
 * A webview cannot open a Unix domain socket, so the connection lives in the
 * Tauri shell (`src-tauri/src/daemon.rs`) and this module is a thin call
 * surface over it: Tauri commands out, a Tauri channel of daemon frames back.
 * There is no wire codec here — every frame that crosses the boundary was
 * produced by the shared Rust protocol crate (adoption 14 §6, criterion 2).
 *
 * `createTransport()` returns `null` when the app is not running inside the
 * shell (a plain `vite dev` browser tab, or a test). That is deliberate: with
 * no shell there is no transport, and the UI must say so rather than pretend.
 */
import { Channel, invoke } from "@tauri-apps/api/core";
import { desktopManifest } from "./agui/types.js";
import type { ApprovalGrant } from "./approval.js";
import type {
  CodeGraphPage,
  CodeGraphQuery,
  CodeGraphScanReport,
  CodeGraphStatusView,
} from "@codypendent/protocol";
import type {
  AgentMode,
  AnalyticsExportRequest,
  AnalyticsExportResult,
  AnalyticsPage,
  AnalyticsQuery,
  ArtifactRef,
  BlackboardItemView,
  Catchup,
  InboxEntry,
  InboxListQuery,
  InboxMutation,
  InboxPage,
  PageCursor,
  PromptDelivery,
  SessionEvent,
  SessionLifecycleAction,
  SessionSearchPage,
  SessionSummary,
  WorkflowEvent,
  WorkflowCatalog,
  WorkflowRunSnapshot,
} from "@codypendent/protocol";
import type { QuestionOutcomeView, WorkspaceTarget, WorkspaceOpenRequest } from "./types.js";
import type {
  DocCard,
  DocumentPublishPlan,
  KnowledgeTransport,
  LearningCard,
  LearningMutation,
  MemoryCard,
  SkillCard,
  UiPluginLifecycleStatus,
} from "./components/knowledgeTransport.js";
import type { DocumentLeaseGrant, DocumentMutation } from "@codypendent/protocol";
import type {
  CouncilCard,
  CouncilDraft,
  CouncilProgressFrame,
  CouncilResultCard,
  CouncilResultsPage,
  CouncilRunReply,
  RepositorySelection,
} from "./localConfig.js";
import {
  exportAnalytics,
  listInbox,
  mutateInbox,
  queryAnalytics,
  type CommandExecutor,
  type ProtocolCommandCaller,
} from "@codypendent/protocol";

export type ConnectionInfo = {
  socket_path: string;
  protocol_version: string;
  daemon_version: string;
  daemon_instance: string;
  build_id: string;
  /** This shell's own version. Absent from an older shell. */
  client_version?: string;
};

/**
 * What the shell knows about starting a daemon: whether one answers on the
 * socket, what it would launch, and the command to run by hand when it cannot.
 * Exactly `launcher::LaunchStatus` (`src-tauri/src/launcher.rs`).
 */
export type DaemonLaunchStatus = {
  socketPath: string;
  listening: boolean;
  invocation?: { program: string; args: string[] };
  source?: "override" | "path" | "known-directory" | "beside-this-app" | "development-build";
  manualCommand: string;
  logPath: string;
  searched: string[];
};

/** What `daemon_start` did. */
export type DaemonStartOutcome =
  | { outcome: "already-running" }
  | { outcome: "started"; pid: number; program: string };

export type SessionRow = SessionSummary;

export type RunHandle = {
  session_id: string;
  run_id: string | null;
};

export type SessionEventFrame = SessionEvent;

export type ApprovalChoice = "approve" | "reject";

export type DaemonFrame =
  | { kind: "remote_ui"; session_id: string | null; message: import("@codypendent/protocol").UiWireMessage }
  | { kind: "event"; session_id: string | null; event: SessionEvent }
  | { kind: "catchup"; session_id: string; snapshot: Catchup }
  | { kind: "history"; session_id: string; through: number; events: SessionEvent[] }
  /**
   * A live workflow node transition or run-phase change. Not session-scoped:
   * the event carries its own `workflow_run_id`. A `NodeTransitioned` is
   * full-state, so it merges by `node_id` as an overwrite — but it omits
   * `depends_on`, so a merge must keep the edges the snapshot supplied.
   */
  | { kind: "workflow_event"; event: WorkflowEvent }
  /**
   * A blackboard artifact that just landed on a subscribed board — a workflow
   * run's, or the repository task board (whose `workflow_run_id` is the
   * synthetic `board:<repo>` id). Merges by id; a superseding revision arrives
   * as its own delivery and retires the item it names in `superseded_by`.
   */
  | { kind: "blackboard_posted"; item: BlackboardItemView }
  | { kind: "disconnected"; reason: string };

/**
 * One page of ranked session search, tagged with the query it answers.
 *
 * `Payload::SessionSearchResults` echoes back only the page, and two searches
 * can be in flight at once. Without the query travelling back with its page,
 * the slow answer to an abandoned query lands under the heading of the query
 * since typed. Callers compare `query` against the live search box and discard
 * a mismatch.
 */
export type SessionSearchAnswer = {
  query: string;
  /** The cursor this page continues from; `null` for a first page. */
  cursor: PageCursor | null;
  /** `page.next_cursor` present means the set was CUT, not exhausted. */
  page: SessionSearchPage;
};

/**
 * What a lifecycle mutation actually did. Three different outcomes, kept
 * distinct: a delete's `tombstoned` flag is the daemon's retention decision and
 * the client neither predicts nor overwrites it.
 */
export type SessionLifecycleOutcome =
  | { outcome: "applied"; session: SessionSummary }
  | { outcome: "deleted"; session_id: string; tombstoned: boolean }
  | { outcome: "exported"; artifact: ArtifactRef };

/** The two authoritative baselines a workflow watch establishes. */
export type WorkflowWatch = {
  snapshot: WorkflowRunSnapshot;
  /** Superseded revisions included — the run panel shows correction history. */
  blackboard: BlackboardItemView[];
};

/** The repository task board, plus the checkout it is actually keyed by. */
export type BoardView = {
  /** The git toplevel the board hangs off, echoed so a wrong anchor is visible. */
  repository: string;
  /** `board:<repository>` — the synthetic run id its live channel uses. */
  board_scope_id: string;
  cards: BlackboardItemView[];
};

export type DesktopTransport = {
  sendRemoteUi?(sessionId: string, message: UiWireMessage): Promise<void>;
  /** Where the shell will look for the daemon socket. */
  socketPath(): Promise<string>;
  /** Connect and handshake. Rejects when no daemon answers. */
  /**
   * `generation` identifies THIS attempt, so a deferred teardown cannot close a
   * connection that replaced the one it meant to close. Reconnect defers its
   * disconnect until the previous connect settles, and without this the
   * deferred call took whichever connection was registered by then — silently,
   * because a deliberate disconnect emits no `Disconnected` frame, so the store
   * stayed "connected" while every command timed out.
   */
  connect(onFrame: (frame: DaemonFrame) => void, generation?: number): Promise<ConnectionInfo>;
  /** Close only `generation`; omitted closes whatever is open (app teardown). */
  disconnect(generation?: number): Promise<void>;
  /**
   * Whether a daemon answers on the socket, and what the shell could start.
   * Optional: an older shell has neither this nor `startDaemon`, and the
   * banner then names the manual command instead of offering a button.
   */
  daemonLaunchStatus?(): Promise<DaemonLaunchStatus>;
  /**
   * Start `codypendentd` detached and wait for its socket. Resolves once a
   * daemon answers (started here, or already running); rejects with a message
   * that names what was tried and the command to run by hand.
   */
  startDaemon?(): Promise<DaemonStartOutcome>;
  listSessions(): Promise<SessionSummary[]>;
  /** Start a run in the confirmed session, or create one when sessionId is null. */
  startObjective(objective: string, sessionId?: string | null): Promise<RunHandle>;
  /** Attach to an existing session and replay its catch-up. */
  attachSession(sessionId: string): Promise<void>;
  /**
   * The durable events in `(afterSequence, through]`.
   *
   * Used to repair a live-stream gap: a jump in `sequence` means events this
   * client never received, and the transcript is short by exactly that range
   * until they are read back from the log.
   */
  readSessionEventRange?(
    sessionId: string,
    afterSequence: number,
    through: number,
  ): Promise<SessionEvent[]>;
  /** Send a real `CancelRun`. */
  cancelRun(runId: string): Promise<void>;
  /**
   * Send a real `QueueSteering` for a live run — redirect it without killing
   * it. Optional because a transport stub (or an older shell) may not offer
   * the `queue_steering` command; the steering panel then says so instead of
   * pretending the text went anywhere.
   *
   * Resolving means the daemon ACCEPTED the command. It does NOT mean the
   * steering is queued, and it certainly does not mean it was applied — those
   * are the `SteeringQueued` and `SteeringApplied` events on the session
   * stream, and only they may be rendered as such.
   */
  queueSteering?(runId: string, text: string): Promise<void>;
  /** Resolve a daemon-owned pending approval for this attached client. */
  resolveApproval(approvalId: string, decision: ApprovalChoice, scope?: ApprovalGrant): Promise<void>;
  /**
   * Answer or reject a parked question (`ResolveQuestion`). Optional because
   * an older shell has no handler; the card then says the run is waiting and
   * that this client cannot answer.
   */
  resolveQuestion?(questionId: string, outcome: QuestionOutcomeView): Promise<void>;
  /** List notifications and human work from the durable inbox. */
  listInbox(query?: InboxListQuery): Promise<InboxPage>;
  /** Apply an idempotent mutation (Acknowledge, Dismiss) to an inbox entry. */
  mutateInbox(mutation: InboxMutation): Promise<InboxEntry>;
  /** Query measured execution observations and aggregates. */
  queryAnalytics(query?: AnalyticsQuery): Promise<AnalyticsPage>;
  /** Export measured analytics as bounded JSON or CSV artifact. */
  exportAnalytics(request: AnalyticsExportRequest): Promise<AnalyticsExportResult>;
  /**
   * Send a real `PauseRun` — stop a live run without killing it.
   *
   * Which states admit a pause is the DAEMON's rule
   * (`validate_run_transition`, `crates/daemon/src/commands.rs`), not this
   * client's, and a refusal arrives as a thrown `run.invalid-transition`. The
   * UI still hides the button unless the run state it folded off
   * `RunStateChanged` says the run can take it, so an operator is never offered
   * a button whose only possible outcome is an error.
   *
   * Optional because an older shell has no `pause_run` handler; the run
   * controls then say so rather than pretending.
   */
  pauseRun?(runId: string): Promise<void>;
  /**
   * Send a real `ResumeRun`. The daemon admits this ONLY from `Paused`, so the
   * UI offers it only when the folded run state IS `Paused`.
   */
  resumeRun?(runId: string): Promise<void>;

  // ------------------------------------------------- Pending-prompt queue
  //
  // `QueuePrompt` / `UpdateQueuedPrompt` / `PromoteQueuedPrompt` /
  // `DeleteQueuedPrompt` (adoption 06), all scoped by the shell to the ATTACHED
  // session — the webview cannot name a session it is not looking at.
  //
  // None of them return the queue. The daemon appends a full
  // `PendingPromptsChanged` snapshot to the session stream in the same
  // transaction, and that event is the only thing the store folds; resolving
  // here means the command was ACCEPTED and nothing more.

  /**
   * Queue a follow-up prompt on the attached session.
   *
   * `mode` omitted uses the mode staged in the mode picker (the shell's
   * `RunDefaults`), exactly as `startObjective` does — the queue never invents
   * a mode of its own.
   */
  queuePrompt?(text: string, delivery: PromptDelivery, mode?: AgentMode): Promise<void>;
  /** Edit a queued prompt in place; absent fields keep their values. */
  updateQueuedPrompt?(
    promptId: string,
    text?: string | null,
    delivery?: PromptDelivery | null,
  ): Promise<void>;
  /** Promote a queued prompt to `Steer` and move it to the front. */
  promoteQueuedPrompt?(promptId: string): Promise<void>;
  /** Remove a queued prompt without ever running it. */
  deleteQueuedPrompt?(promptId: string): Promise<void>;

  /**
   * Read an artifact's whole content.
   *
   * Takes the `ArtifactRef` rather than a bare id because `ReadArtifact` binds
   * every chunk request to the digest the client observed
   * (`crates/protocol/src/command.rs`); an id alone cannot form the command.
   * Yields bytes, not text: an artifact may be a patch, a CSV or audio, so the
   * caller decodes with `TextDecoder` when it knows the media type is textual.
   */
  readArtifact?(artifact: ArtifactRef): Promise<Uint8Array>;
  /** Native Save dialog; null means cancelled, a path means verified bytes saved. */
  saveArtifact?(artifact: ArtifactRef): Promise<string | null>;

  /**
   * Ranked session search. The reply carries the query it answers; a caller
   * whose search box has moved on must drop it rather than render it.
   *
   * A rejection surfaces as a thrown error — a FAILED search, which is not the
   * same fact as an empty page and must not be drawn as one.
   */
  searchSessions?(query: string, cursor?: PageCursor | null): Promise<SessionSearchAnswer>;
  /** Rename / pin / unpin / archive / restore / delete / export one session. */
  mutateSession?(sessionId: string, action: SessionLifecycleAction): Promise<SessionLifecycleOutcome>;

  /** Start a durable workflow run by id; `inputs` must be a JSON object. */
  startWorkflow?(workflowId: string, inputs: Record<string, unknown>, options?: { expectedRepository?: string; idempotencyKey?: string; expectedDefinitionDigest?: string }): Promise<string>;
  /** Read the owning runtime's effective definitions for the captured repository. */
  listWorkflows?(expectedRepository?: string): Promise<WorkflowCatalog>;
  /** A run's snapshot on its own, for a refresh that does not re-subscribe. */
  readWorkflowRun?(workflowRunId: string): Promise<WorkflowRunSnapshot>;
  /** Subscribe to a run's live streams and read both baselines. */
  watchWorkflow?(workflowRunId: string): Promise<WorkflowWatch>;
  pauseWorkflow?(workflowRunId: string): Promise<void>;
  resumeWorkflow?(workflowRunId: string): Promise<void>;
  /** Cancel a run — terminal on the daemon side, so confirm before calling. */
  cancelWorkflow?(workflowRunId: string): Promise<void>;
  retryWorkflowNode?(workflowRunId: string, nodeId: string): Promise<void>;

  /** A run's board, superseded revisions included. */
  readBlackboard?(workflowRunId: string): Promise<BlackboardItemView[]>;
  /** Post an open question — the only kind an operator may post. */
  postBlackboardQuestion?(workflowRunId: string, text: string): Promise<BlackboardItemView>;

  /** Subscribe to the repository task board and read its live cards. */
  watchBoard?(): Promise<BoardView>;
  createBoardCard?(title: string): Promise<BlackboardItemView>;
  /** Move a card; the daemon supersedes it and returns the replacement. */
  moveBoardCard?(itemId: string, status: string): Promise<BlackboardItemView>;

  /**
   * Open the OS folder picker and select a repository.
   *
   * Resolves to `null` when the operator dismissed the dialog, and REJECTS when
   * the folder was refused — a folder that is not a git checkout, or the home
   * directory. Those are different outcomes and the UI must not merge them.
   */
  pickRepository?(): Promise<RepositorySelection | null>;
  /** The repository currently selected; `null` when none is. */
  currentRepository?(): Promise<RepositorySelection | null>;
  /** Select by path, through the same gate the picker uses. */
  setRepository?(path: string): Promise<RepositorySelection>;
  clearRepository?(): Promise<void>;
  workspaceTargets?(): Promise<WorkspaceTarget[]>;
  openWorkspace?(request: WorkspaceOpenRequest): Promise<void>;

  /** Every configured council, from `councils.toml`. */
  listCouncils?(): Promise<CouncilCard[]>;
  /** Persist a new council; rejects with the council crate's own refusal text. */
  createCouncil?(draft: CouncilDraft): Promise<CouncilCard>;
  /** Remove a definition. Saved run reports stay on disk. */
  deleteCouncil?(name: string): Promise<void>;
  /** Every council's newest durable result, plus per-council read warnings. */
  listCouncilResults?(): Promise<CouncilResultsPage>;
  /** One result by council name or result id; `null` when there is none. */
  councilResult?(selector: string): Promise<CouncilResultCard | null>;
  /**
   * Convene a council. Settles when the deliberation does — minutes, not
   * milliseconds — while `onProgress` receives each round/member/chair
   * transition in the meantime.
   */
  runCouncil?(
    name: string,
    objective: string,
    options: { repository?: string | null; sessionId?: string | null },
    onProgress: (frame: CouncilProgressFrame) => void,
  ): Promise<CouncilRunReply>;

  // ---------------------------------------------------------------- Code graph
  //
  // `ReadCodeGraphStatus` / `ReadCodeGraph`, scoped by the shell to the
  // connection's anchored checkout — the webview cannot name a repository, and
  // the daemon resolves the path to its enclosing checkout itself.

  /** What the STORED graph holds right now, with no re-scan. */
  codeGraphStatus?(): Promise<CodeGraphStatusView>;
  /** Rebuild the selected checkout's stored map; the daemon enforces Controller access. */
  buildCodeGraph?(): Promise<CodeGraphScanReport>;
  /**
   * One FILTERED, LIMITED page of nodes and edges.
   *
   * Always pass a `limit`: a real graph is ~500k nodes and 1.2M edges, and a
   * `limit` of 0 asks for the daemon's ceiling rather than for everything. The
   * reply's `total_nodes` / `total_edges` are computed before the limit, so a
   * caller renders "showing N of M" and never implies it showed the whole set.
   * There is no cursor and no offset — a cut page is narrowed, not paged past.
   */
  readCodeGraph?(query: CodeGraphQuery): Promise<CodeGraphPage>;

  // ----------------------------------------------------------- Backtrack
  //
  // `ForkSession` / `RestoreCheckpoint`. Both are Controller-only and both are
  // gated by the daemon, whose refusals surface as thrown errors carrying its
  // own message and code.

  /**
   * Fork the ATTACHED session at a run-launch checkpoint; resolves to the new
   * session's id. The source session is never modified.
   */
  forkSession?(checkpoint: string, name?: string | null): Promise<string>;
  /**
   * Ask to rewind a settled run's worktree to a recorded checkpoint.
   *
   * Resolving means the daemon ACCEPTED the request and parked its own
   * high-risk approval — not that anything was restored. The restore happens
   * only if a human approves that card, and `CheckpointRestored { restored }`
   * is what says whether it did.
   */
  restoreCheckpoint?(runId: string, checkpoint: string): Promise<void>;
};

export type { CommandExecutor, ProtocolCommandCaller };
export { exportAnalytics, listInbox, mutateInbox, queryAnalytics };

/** True only inside the Tauri shell, where the bridge commands exist. */
export function shellAvailable(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/**
 * The transport backed by the Tauri shell, or `null` when there is no shell.
 */
export function createTransport(): DesktopTransport | null {
  if (!shellAvailable()) {
    return null;
  }
  return {
    socketPath: () => invoke<string>("daemon_socket"),
    connect: (onFrame, generation) => {
      const channel = new Channel<DaemonFrame>();
      channel.onmessage = onFrame;
      return invoke<ConnectionInfo>("daemon_connect", { channel, generation });
    },
    disconnect: (generation) => invoke<void>("daemon_disconnect", { generation }),
    daemonLaunchStatus: () => invoke<DaemonLaunchStatus>("daemon_launch_status"),
    startDaemon: () => invoke<DaemonStartOutcome>("daemon_start"),
    listSessions: () => invoke<SessionSummary[]>("list_sessions"),
    startObjective: (objective, sessionId) => invoke<RunHandle>("start_objective", { objective, sessionId: sessionId ?? null, desktop: desktopManifest() }),
    attachSession: (sessionId) => invoke<void>("attach_session", { sessionId }),
    readSessionEventRange: (sessionId, afterSequence, through) =>
      invoke<SessionEvent[]>("read_session_event_range", { sessionId, afterSequence, through }),
    cancelRun: (runId) => invoke<void>("cancel_run", { runId }),
    queueSteering: (runId, text) => invoke<void>("queue_steering", { runId, text }),
    resolveApproval: (approvalId, decision, scope = "Once") =>
      invoke<void>("resolve_approval", { approvalId, approved: decision === "approve", scope: { type: scope } }),
    resolveQuestion: (questionId, outcome) =>
      invoke<void>("resolve_question", { questionId, outcome }),
    pauseRun: (runId) => invoke<void>("pause_run", { runId }),
    resumeRun: (runId) => invoke<void>("resume_run", { runId }),
    queuePrompt: (text, delivery, mode) =>
      invoke<void>("queue_prompt", { text, delivery, mode: mode ?? null }),
    updateQueuedPrompt: (promptId, text, delivery) =>
      invoke<void>("update_queued_prompt", {
        promptId,
        text: text ?? null,
        delivery: delivery ?? null,
      }),
    promoteQueuedPrompt: (promptId) => invoke<void>("promote_queued_prompt", { promptId }),
    deleteQueuedPrompt: (promptId) => invoke<void>("delete_queued_prompt", { promptId }),
    listInbox: (query) => invoke<InboxPage>("list_inbox", { query }),
    mutateInbox: (mutation) => invoke<InboxEntry>("mutate_inbox", { mutation }),
    queryAnalytics: (query) => invoke<AnalyticsPage>("query_analytics", { query }),
    exportAnalytics: (request) => invoke<AnalyticsExportResult>("export_analytics", { request }),
    // The shell answers with a raw IPC body, which Tauri delivers to the
    // webview as an `ArrayBuffer`; the bytes are the daemon's, verified against
    // `artifact` in the shell before they get here.
    sendRemoteUi: (sessionId, message) => invoke<void>("send_remote_ui", { sessionId, message }),
    readArtifact: async (artifact) =>
      new Uint8Array(await invoke<ArrayBuffer>("read_artifact", { artifact })),
    saveArtifact: (artifact) => invoke<string | null>("save_artifact", { artifact }),

    searchSessions: (query, cursor) =>
      invoke<SessionSearchAnswer>("search_sessions", { query, cursor: cursor ?? null }),
    mutateSession: (sessionId, action) =>
      invoke<SessionLifecycleOutcome>("mutate_session", { sessionId, action }),

    listWorkflows: (expectedRepository) => invoke<WorkflowCatalog>("list_workflows", { expectedRepository }),
    startWorkflow: (workflowId, inputs, options) => invoke<string>("start_workflow", { workflowId, inputs, ...options }),
    readWorkflowRun: (workflowRunId) =>
      invoke<WorkflowRunSnapshot>("read_workflow_run", { workflowRunId }),
    watchWorkflow: (workflowRunId) => invoke<WorkflowWatch>("watch_workflow", { workflowRunId }),
    pauseWorkflow: (workflowRunId) => invoke<void>("pause_workflow", { workflowRunId }),
    resumeWorkflow: (workflowRunId) => invoke<void>("resume_workflow", { workflowRunId }),
    cancelWorkflow: (workflowRunId) => invoke<void>("cancel_workflow", { workflowRunId }),
    retryWorkflowNode: (workflowRunId, nodeId) =>
      invoke<void>("retry_workflow_node", { workflowRunId, nodeId }),

    readBlackboard: (workflowRunId) =>
      invoke<BlackboardItemView[]>("read_blackboard", { workflowRunId }),
    postBlackboardQuestion: (workflowRunId, text) =>
      invoke<BlackboardItemView>("post_blackboard_question", { workflowRunId, text }),

    watchBoard: () => invoke<BoardView>("watch_board"),
    createBoardCard: (title) => invoke<BlackboardItemView>("create_board_card", { title }),
    moveBoardCard: (itemId, status) =>
      invoke<BlackboardItemView>("move_board_card", { itemId, status }),

    pickRepository: () => invoke<RepositorySelection | null>("pick_repository"),
    currentRepository: () => invoke<RepositorySelection | null>("current_repository"),
    workspaceTargets: () => invoke<WorkspaceTarget[]>("workspace_targets"),
    openWorkspace: (request) => invoke<void>("workspace_open", { ...request }),
    setRepository: (path) => invoke<RepositorySelection>("set_repository", { path }),
    clearRepository: () => invoke<void>("clear_repository"),

    listCouncils: () => invoke<CouncilCard[]>("list_councils"),
    createCouncil: (draft) => invoke<CouncilCard>("create_council", { draft }),
    deleteCouncil: (name) => invoke<void>("delete_council", { name }),
    listCouncilResults: () => invoke<CouncilResultsPage>("list_council_results"),
    councilResult: (selector) => invoke<CouncilResultCard | null>("council_result", { selector }),
    runCouncil: (name, objective, options, onProgress) => {
      // Progress rides its own channel because the command future does not
      // settle until the whole deliberation has. Without it the UI would show
      // nothing at all for the several minutes a multi-round council takes.
      const channel = new Channel<CouncilProgressFrame>();
      channel.onmessage = onProgress;
      return invoke<CouncilRunReply>("run_council", {
        name,
        objective,
        repository: options.repository ?? null,
        sessionId: options.sessionId ?? null,
        channel,
      });
    },

    codeGraphStatus: () => invoke<CodeGraphStatusView>("code_graph_status"),
    buildCodeGraph: () => invoke<CodeGraphScanReport>("build_code_graph"),
    readCodeGraph: (query) => invoke<CodeGraphPage>("read_code_graph", { query }),

    forkSession: (checkpoint, name) =>
      invoke<string>("fork_session", { checkpoint, name: name ?? null }),
    restoreCheckpoint: (runId, checkpoint) =>
      invoke<void>("restore_checkpoint", { runId, checkpoint }),
  };
}

/**
 * The knowledge surfaces' transport, or `null` when there is no shell.
 *
 * The lists are the shell's own reads of the daemon's database
 * (`src-tauri/src/knowledge.rs`, the mapping the TUI harness performs); the
 * mutations are protocol commands on the live connection (`daemon.rs`). Two
 * of the document operations are sequences, composed here exactly as the
 * TUI's reducer composes them: acquire the lease, apply the mutation, release
 * the lease — the block's own lease for a text edit, the whole document's for
 * a structural delete.
 */
export function createKnowledgeTransport(): KnowledgeTransport | null {
  if (!shellAvailable()) {
    return null;
  }
  const plugins = (command: string, args?: Record<string, unknown>) =>
    invoke<UiPluginLifecycleStatus[]>(command, args);
  return {
    listSkills: () => invoke<SkillCard[]>("list_skills"),
    listMemories: () => invoke<MemoryCard[]>("list_memories"),
    correctMemory: (memoryId, statement) =>
      invoke<string>("correct_memory", { memoryId, statement }),
    forgetMemory: (memoryId) => invoke<string>("forget_memory", { memoryId }),
    listLearnings: () => invoke<LearningCard[]>("list_learnings"),
    mutateLearning: (learningId, revision, mutation: LearningMutation) =>
      invoke<string>("mutate_learning", { learningId, revision, mutation }),
    listDocuments: () => invoke<DocCard[]>("list_documents"),
    createDocument: (title) => invoke<string>("create_document", { title }),
    replaceDocumentBlock: (documentId, blockId, original, replacement) =>
      underLease(
        documentId,
        blockId,
        {
          op: "edit_text",
          block_id: blockId,
          position: 0,
          // The daemon counts characters as Unicode scalar values, as
          // `str::chars` does; `Array.from` splits a string the same way,
          // where `.length` would count UTF-16 units and over-delete past an
          // emoji or a CJK ideograph outside the BMP.
          delete_len: Array.from(original).length,
          insert: replacement,
        },
        // This mutation is a FULL REPLACEMENT: it deletes `original`'s length
        // from position zero and inserts the new text. The editor compared
        // against its cached projection before submitting, but document writes
        // are not streamed into that projection and the lease is only taken
        // here — so another writer's edit in that window would be deleted.
        // Re-read once the lease is HELD, which is the only point at which the
        // text is known not to move before the mutation lands.
        async () => {
          const documents = await invoke<DocCard[]>("list_documents");
          const block = documents
            .find((doc) => doc.document_id === documentId)
            ?.blocks.find((candidate) => candidate.id === blockId);
          if (block === undefined) {
            throw new Error(
              "this block is no longer in the document; reopen it to see the current text",
            );
          }
          if (block.editable !== original) {
            throw new Error(
              "this block changed since you opened it; reopen it so your edit does not " +
                "overwrite someone else's",
            );
          }
        },
      ),
    deleteDocumentBlock: (documentId, blockId) =>
      underLease(documentId, null, { op: "delete", block_id: blockId }),
    publishDocument: (documentId, target) =>
      invoke<DocumentPublishPlan>("publish_document", { documentId, target }),
    listUiPlugins: () => plugins("list_ui_plugins"),
    smokeTestUiPlugin: (pluginId) => plugins("smoke_test_ui_plugin", { pluginId }),
    enableUiPlugin: (pluginId, scope) => plugins("enable_ui_plugin", { pluginId, scope }),
    approveUiPluginUpdate: (pluginId, approvalReceipt) =>
      plugins("approve_ui_plugin_update", { pluginId, approvalReceipt }),
    rejectUiPluginUpdate: (pluginId, approvalReceipt) =>
      plugins("reject_ui_plugin_update", { pluginId, approvalReceipt }),
    revokeUiPlugin: (pluginId) => plugins("revoke_ui_plugin", { pluginId }),
  };
}

/**
 * Apply one mutation under a lease: acquire, mutate, release.
 *
 * `blockId` is the block a text edit leases; `null` leases the whole document
 * structure, which a block delete needs. The release always runs — a lease
 * left behind blocks that range for every other writer until it expires —
 * and when both the mutation and the release fail, the mutation's error is
 * the one reported, because it is the one the operator acted on.
 *
 * `verify` runs once the lease is HELD and before the mutation, so a caller
 * whose mutation assumes the current text (a full replacement does) can prove
 * that assumption at the only moment it is guaranteed to keep holding. It
 * throws to abort, and the lease is released either way.
 */
async function underLease(
  documentId: string,
  blockId: string | null,
  mutation: DocumentMutation,
  verify?: () => Promise<void>,
): Promise<void> {
  const grant = await invoke<DocumentLeaseGrant>("acquire_document_lease", { documentId, blockId });
  let failure: unknown = null;
  let failed = false;
  try {
    // Inside the try, so a refusal here still releases the lease.
    if (verify) {
      await verify();
    }
    await invoke<void>("mutate_document", { documentId, mutation });
  } catch (error) {
    failed = true;
    failure = error;
  }
  try {
    await invoke<void>("release_document_lease", { leaseId: grant.lease_id });
  } catch (error) {
    if (!failed) {
      throw error;
    }
  }
  if (failed) {
    throw failure;
  }
}
