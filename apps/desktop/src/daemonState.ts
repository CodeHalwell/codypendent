/**
 * The desktop client's store: daemon frames in, transcript out.
 *
 * Every transcript item here is derived from authoritative daemon state: a
 * durable `SessionEvent`, or actionable projection data in a compact catch-up
 * snapshot. There is no branch that produces assistant text, a run, or a
 * session from client-side state — if the daemon said nothing, the transcript
 * stays empty. The one client-owned field is `error`, which reports a failed
 * *command*, and is rendered as a client error, never as agent output.
 */
import type {
  ArtifactRef,
  Catchup,
  InboxEntry,
  PendingPromptView,
  RunState,
  SessionEvent,
} from "@codypendent/protocol";
import type {
  ConnectionInfo,
  DaemonFrame,
  RunHandle,
  SessionRow,
} from "./transport.js";
import type {
  ConnectionStatus,
  QuestionPromptView,
  RunActivity,
  RunUsage,
  SessionSummary,
  TranscriptItem,
} from "./types.js";
import { diagnoseFailure, sanitizeFailureText } from "./failure.js";
import type { StructuredFailure } from "./failure.js";

export interface DaemonState {
  status: ConnectionStatus;
  /** Why the client is in this state, in words an operator can act on. */
  detail: string;
  info: ConnectionInfo | null;
  /**
   * How many times a connection has been established, counting from one.
   *
   * A reconnect builds a NEW client, and a subscription belongs to the
   * connection that asked for it — so every live watch a panel grew (a
   * workflow's node transitions, a blackboard's posts) is gone once the socket
   * is replaced, with nothing on screen to say the panel has stopped updating.
   * Panels holding a watch re-establish it when this changes. It counts
   * connections rather than flagging a boolean so that a panel opened during a
   * reconnect cannot miss the transition.
   */
  connectionEpoch: number;
  /**
   * A hole in the live event stream that has not been repaired yet.
   *
   * A jump in `sequence` means events this client never received — a lagging
   * subscriber, a frame dropped under load. It used to be detected, logged to
   * the console and then forgotten, which left the transcript permanently
   * short by that range with nothing on screen marking where. Recorded here so
   * the shell can read the range back from the durable log; cleared once it
   * has. Widened rather than replaced when a second gap opens before the first
   * is repaired, so no hole is lost by being overtaken.
   */
  pendingGap: { sessionId: string; after: number; through: number } | null;
  sessions: SessionSummary[];
  /**
   * The session currently being attached, before the daemon has accepted it.
   *
   * `activeSessionId` remains the last confirmed attachment until this clears.
   * Session-scoped controls use this field to stay disabled during the handoff:
   * the daemon changes its attachment before the shell promise resolves, so
   * commands sent in that interval cannot be targeted safely.
   */
  attachingSessionId: string | null;
  /** Whether the current native connection has confirmed `activeSessionId`. */
  sessionAttachmentConfirmed: boolean;
  activeSessionId: string | null;
  activeRunId: string | null;
  isRunning: boolean;
  transcript: TranscriptItem[];
  /** Durable events retained for sequence de-duplication and history rebuilds. */
  durableEvents: SessionEvent[];
  lastSequence: number;
  /** The last command that failed, if any. Cleared on the next submit. */
  error: string | null;
  /** Durable inbox entries. */
  inbox: InboxEntry[];
  /** Count of unread inbox items. */
  unreadInboxCount: number;
  /**
   * Whether `inbox` reflects a real answer from the daemon.
   *
   * An empty `inbox` is ambiguous on its own: it is the same array whether the
   * daemon said "nothing pending" or was never asked. Only `"loaded"` licenses
   * the UI to state that there is no pending work.
   */
  inboxStatus: "unloaded" | "loaded" | "unavailable";
  /** Why the inbox could not be read, when `inboxStatus` is `"unavailable"`. */
  inboxDetail: string | null;
  /**
   * The daemon's OWN lifecycle state for `activeRunId`, or `null` when this
   * client cannot tell.
   *
   * Folded from `RunStateChanged` — the same event the daemon appends when it
   * moves a run (`crates/daemon/src/ledger.rs::append_run_state_changed`) and
   * when it applies `PauseRun`/`ResumeRun`
   * (`commands.rs::apply_run_state`). `null` is a real answer and is used as
   * one: a >500-event catch-up arrives as a `SessionProjection`, which carries
   * `active_runs` but NO run state, so pause/resume are not offered at all
   * rather than guessed at. Never inferred from `isRunning`.
   */
  runState: RunState["type"] | null;
  /**
   * The session's server-side pending-prompt queue, exactly as the daemon last
   * reported it.
   *
   * Latest-wins: `PendingPromptsChanged` carries the WHOLE queue after every
   * mutation, so folding it REPLACES this array (that is the contract in
   * `crates/protocol/src/events.rs`). Also seeded from a compact catch-up's
   * `projection.pending_prompts`.
   */
  pendingPrompts: PendingPromptView[];
  /**
   * Why the last queue mutation FAILED, if it did.
   *
   * Kept apart from an empty `pendingPrompts` on purpose: "the daemon refused
   * this command" and "there is nothing queued" are different facts, and the
   * queue panel renders them differently. Cleared when a mutation is accepted.
   */
  promptQueueError: string | null;
  /** What the run on screen is doing right now. `idle` when there is none. */
  activity: RunActivity;
  /**
   * What the provider measured for the most recent run on screen, once its
   * `RunUsage` arrived. Cleared when the next run starts, so the strip never
   * attributes one run's tokens to another.
   */
  usage: RunUsage | null;
  /** The objective the run on screen was started with, for a failure's Retry. */
  activeObjective: string | null;
  /**
   * Every live run's objective, BY RUN ID.
   *
   * A session runs several runs at once, and a failure's Retry must resubmit
   * the objective of the run that failed. `activeObjective` alone cannot do
   * that: a sibling starting is deliberately not allowed to hijack the
   * surface, so its objective is never staged there — and once the displayed
   * run finishes and a later event adopts the sibling, `activeObjective` still
   * holds the OLD run's text. Retry would then launch the wrong work, and pay
   * for it. Entries are dropped as their runs complete.
   */
  objectivesByRun: Record<string, string>;
}

export type DaemonAction =
  | { type: "shell-missing"; detail: string }
  | { type: "connecting"; detail: string }
  | { type: "connected"; info: ConnectionInfo }
  | { type: "connect-failed"; detail: string }
  | { type: "sessions"; sessions: SessionRow[] }
  | { type: "session-attach-started"; sessionId: string }
  | { type: "session-attach-failed"; sessionId: string }
  | { type: "session-selected"; sessionId: string }
  | { type: "new-session" }
  | { type: "session-reattached"; sessionId: string }
  | { type: "session-history-incomplete"; sessionId: string; through: number }
  | { type: "run-submitted"; handle: RunHandle; objective: string }
  | { type: "command-failed"; message: string | null }
  | { type: "inbox-loaded"; entries: InboxEntry[] }
  | { type: "inbox-unavailable"; detail: string }
  | { type: "inbox-entry-updated"; entry: InboxEntry }
  /** A queue mutation the daemon refused, or that never reached it. */
  | { type: "prompt-queue-failed"; detail: string }
  /** A queue mutation the daemon accepted; retires any previous failure. */
  | { type: "prompt-queue-accepted" }
  /** The recorded gap has been read back from the log and folded in. */
  | { type: "gap-repaired"; sessionId: string; through: number }
  | { type: "frame"; frame: DaemonFrame };

export const initialState: DaemonState = {
  status: "disconnected",
  connectionEpoch: 0,
  pendingGap: null,
  detail: "No connection attempted yet.",
  info: null,
  sessions: [],
  attachingSessionId: null,
  sessionAttachmentConfirmed: false,
  activeSessionId: null,
  activeRunId: null,
  isRunning: false,
  transcript: [],
  durableEvents: [],
  lastSequence: 0,
  error: null,
  inbox: [],
  unreadInboxCount: 0,
  inboxStatus: "unloaded",
  inboxDetail: null,
  runState: null,
  pendingPrompts: [],
  promptQueueError: null,
  activity: { kind: "idle" },
  usage: null,
  activeObjective: null,
  objectivesByRun: {},
};

/**
 * The one run-lifecycle control the daemon would actually accept right now, or
 * `null` for neither.
 *
 * This is a TRANSCRIPTION of `validate_run_transition`
 * (`crates/daemon/src/commands.rs`), which is the only authority on the matter:
 *
 *   - `PauseRun`  — legal from any live, not-already-`Paused`, not-`Unknown`
 *     state. Terminal states (`Completed`/`Failed`/`Cancelled`) are refused.
 *   - `ResumeRun` — legal ONLY from `Paused`. "Resuming means leave `Paused`;
 *     anything else is already live or done."
 *
 * The listed live states are therefore enumerated positively rather than
 * derived by negation: a state tag this build has never heard of (a newer
 * daemon's, arriving as `Unknown` or as a literal it cannot classify) yields
 * `null`, and the UI offers nothing. That is the whole point — a client that
 * cannot tell whether a run is pausable must not offer the button.
 */
export function runLifecycleAffordance(
  state: Pick<
    DaemonState,
    "status" | "attachingSessionId" | "sessionAttachmentConfirmed" | "activeRunId" | "runState"
  >,
): "pause" | "resume" | null {
  if (
    state.status !== "connected" ||
    state.attachingSessionId !== null ||
    !state.sessionAttachmentConfirmed ||
    state.activeRunId === null ||
    state.runState === null
  ) {
    return null;
  }
  if (state.runState === "Paused") {
    return "resume";
  }
  const pausable: ReadonlyArray<RunState["type"]> = [
    "Queued",
    "Preparing",
    "Running",
    "WaitingForApproval",
    "WaitingForUserInput",
    "Recovering",
  ];
  return pausable.includes(state.runState) ? "pause" : null;
}

export function reduce(state: DaemonState, action: DaemonAction): DaemonState {
  switch (action.type) {
    case "shell-missing":
    case "connect-failed":
      return {
        ...state,
        status: "disconnected",
        detail: action.detail,
        info: null,
        attachingSessionId: null,
        sessionAttachmentConfirmed: false,
        activeRunId: null,
        isRunning: false,
        runState: null,
        activity: IDLE,
        // Without a daemon the inbox is unreadable, not empty.
        inboxStatus: "unavailable",
        inboxDetail: action.detail,
      };

    case "connecting":
      return {
        ...state,
        status: "connecting",
        detail: action.detail,
        attachingSessionId: null,
        sessionAttachmentConfirmed: false,
      };

    case "gap-repaired":
      // Only clears a gap the repair actually covered: a newer, wider gap may
      // have opened while the read was in flight. The session identity is
      // equally important: a late repair for A must never clear a gap in B.
      return state.pendingGap &&
        state.pendingGap.sessionId === action.sessionId &&
        state.pendingGap.through <= action.through
        ? { ...state, pendingGap: null }
        : state;

    case "connected":
      return {
        ...state,
        status: "connected",
        connectionEpoch: state.connectionEpoch + 1,
        info: action.info,
        detail: `codypendentd ${action.info.daemon_version} on ${action.info.socket_path}`,
      };

    case "sessions":
      return {
        ...state,
        sessions: action.sessions.map((session) => ({
          id: session.session_id,
          title: session.title,
          state: session.state,
          created_at: session.created_at,
          updated_at: session.updated_at,
        })),
      };

    case "session-attach-started":
      return { ...state, attachingSessionId: action.sessionId };

    case "session-attach-failed":
      return state.attachingSessionId === action.sessionId
        ? { ...state, attachingSessionId: null }
        : state;

    case "session-selected":
      return resetSessionProjection(state, action.sessionId);

    case "new-session":
      return { ...resetSessionProjection(state, null), sessionAttachmentConfirmed: false, error: null };

    case "session-reattached":
      return state.activeSessionId === action.sessionId
        ? {
            ...state,
            attachingSessionId: null,
            sessionAttachmentConfirmed: true,
          }
        : resetSessionProjection(state, action.sessionId);

    case "session-history-incomplete": {
      if (state.activeSessionId !== action.sessionId || action.through === 0) {
        return state;
      }
      const pending = state.pendingGap;
      return {
        ...state,
        pendingGap: {
          sessionId: action.sessionId,
          after: 0,
          through: pending?.sessionId === action.sessionId
            ? Math.max(pending.through, action.through)
            : action.through,
        },
      };
    }

    case "run-submitted": {
      const base = state.activeSessionId === action.handle.session_id
        ? state
        : resetSessionProjection(state, action.handle.session_id);
      // StartRun's live events can beat its command reply. Once the run was
      // observed, its replayed lifecycle/activity outrank provisional launch
      // defaults (including a run that already completed before the reply).
      if (action.handle.run_id !== null && runWasObserved(base, action.handle.run_id)) {
        return { ...base, attachingSessionId: null, sessionAttachmentConfirmed: true, error: null };
      }
      return {
        ...base,
        attachingSessionId: null,
        sessionAttachmentConfirmed: true,
        activeSessionId: action.handle.session_id,
        // A null run id is a real answer, not "keep the last one": a run this
        // client cannot name cannot be cancelled or steered, and leaving those
        // controls pointed at the PREVIOUS run would target a run this
        // submission did not start. The run is still live, so `isRunning`
        // stays true.
        activeRunId: action.handle.run_id,
        isRunning: true,
        error: null,
        // The daemon accepted the objective: the run is now being prepared,
        // and its `RunStarted` will confirm the objective from the ledger.
        activity: { kind: "thinking" },
        activeObjective: action.objective,
        usage: null,
      };
    }

    case "command-failed":
      return { ...state, error: action.message };

    case "inbox-loaded": {
      const unreadInboxCount = action.entries.filter(
        (e) => !e.state || e.state.type === "Unread",
      ).length;
      return {
        ...state,
        inbox: action.entries,
        unreadInboxCount,
        inboxStatus: "loaded",
        inboxDetail: null,
      };
    }

    case "inbox-unavailable":
      return {
        ...state,
        inbox: [],
        unreadInboxCount: 0,
        inboxStatus: "unavailable",
        inboxDetail: action.detail,
      };

    case "inbox-entry-updated": {
      const exists = state.inbox.some((e) => e.id === action.entry.id);
      const inbox = exists
        ? state.inbox.map((e) => (e.id === action.entry.id ? action.entry : e))
        : [action.entry, ...state.inbox];
      const unreadInboxCount = inbox.filter(
        (e) => !e.state || e.state.type === "Unread",
      ).length;
      return {
        ...state,
        inbox,
        unreadInboxCount,
      };
    }

    case "prompt-queue-failed":
      // The queue itself is untouched: a refused mutation changed nothing on
      // the daemon, so blanking the projection here would invent a state.
      return { ...state, promptQueueError: action.detail };

    case "prompt-queue-accepted":
      // Accepted, not applied. The queue still only changes when the daemon's
      // own `PendingPromptsChanged` event arrives.
      return { ...state, promptQueueError: null };

    case "frame":
      return applyFrame(state, action.frame);
  }
}

function applyFrame(state: DaemonState, frame: DaemonFrame): DaemonState {
  switch (frame.kind) {
    case "disconnected":
      return {
        ...state,
        status: "disconnected",
        detail: frame.reason,
        info: null,
        attachingSessionId: null,
        sessionAttachmentConfirmed: false,
        activeRunId: null,
        isRunning: false,
        // With no connection there is no run to pause or resume, and no
        // authority for the queue projection either — so neither may linger,
        // claiming a queue the daemon might no longer hold.
        runState: null,
        pendingPrompts: [],
        promptQueueError: null,
        activity: IDLE,
      };
    case "catchup":
      if (state.activeSessionId !== null && state.activeSessionId !== frame.session_id) {
        return state;
      }
      return applySnapshot(
        state.activeSessionId === frame.session_id
          ? state
          : resetSessionProjection(state, frame.session_id),
        frame.session_id,
        frame.snapshot,
      );
    case "history": {
      if (state.activeSessionId !== null && state.activeSessionId !== frame.session_id) {
        return state;
      }
      const base = state.activeSessionId === frame.session_id
        ? state
        : resetSessionProjection(state, frame.session_id);
      const rebuilt = rebuildFromEvents(base, mergeEvents(base.durableEvents, frame.events));
      return {
        ...rebuilt,
        // A compacted page can legitimately carry fewer retained events than
        // its stable watermark. The watermark still establishes where live
        // continuity starts.
        lastSequence: Math.max(rebuilt.lastSequence, frame.through),
      };
    }
    case "event": {
      if (frame.session_id && frame.session_id !== state.activeSessionId) {
        // A live event naming a DIFFERENT session than the attached one is
        // STALE — it arrived during the attach handoff or off a leaked old
        // connection — so it is dropped, never allowed to hijack this
        // projection. Session changes go through `session-selected` (which
        // resets deliberately) and the attach catch-up, so the just-attached
        // session's events already match `activeSessionId` when they arrive.
        if (state.activeSessionId !== null) {
          return state;
        }
        // With nothing attached there is no projection to hijack: adopt the
        // session the event names, exactly as the initial attach does.
        return mergeDurableEvent(resetSessionProjection(state, frame.session_id), frame.event);
      }
      return mergeDurableEvent(
        frame.session_id &&
          frame.session_id === state.activeSessionId &&
          !state.sessionAttachmentConfirmed
          ? { ...state, sessionAttachmentConfirmed: true }
          : state,
        frame.event,
      );
    }
    // Workflow node transitions and blackboard posts are NOT session-scoped:
    // each carries its own `workflow_run_id` and belongs to whichever panel is
    // showing that run or board. This store models one session's transcript, so
    // it has nowhere truthful to put them and deliberately leaves them alone —
    // `frameBus.ts` fans them out to the panels that do model them. Folding
    // them in here would attribute a run's work to whichever session happened
    // to be selected.
    case "remote_ui":
    case "workflow_event":
    case "blackboard_posted":
      return state;
  }
}

function resetSessionProjection(state: DaemonState, sessionId: string | null): DaemonState {
  return {
    ...state,
    attachingSessionId: null,
    sessionAttachmentConfirmed: true,
    activeSessionId: sessionId,
    activeRunId: null,
    isRunning: false,
    transcript: [],
    durableEvents: [],
    lastSequence: 0,
    pendingGap: null,
    // `error` is deliberately left alone. It used to be cleared here, so an
    // operator who saw the red banner and clicked a session to investigate
    // lost the message before reading it. The banner has its own dismiss.
    runState: null,
    pendingPrompts: [],
    promptQueueError: null,
    activity: IDLE,
    usage: null,
    activeObjective: null,
    objectivesByRun: {},
  };
}

function runWasObserved(state: DaemonState, runId: string): boolean {
  if (state.objectivesByRun[runId] !== undefined) return true;
  // Completed runs retire their objective map entry. A fast response can
  // finish before StartRun's reply, so its durable completion still wins.
  for (let index = state.durableEvents.length - 1; index >= 0; index--) {
    const body = state.durableEvents[index].body;
    if (body.type === "RunCompleted" && body.run_id === runId) return true;
  }
  return false;
}

function applySnapshot(
  state: DaemonState,
  sessionId: string,
  snapshot: Catchup,
): DaemonState {
  if (!isProjectionSnapshot(snapshot)) {
    return { ...state, activeSessionId: sessionId };
  }
  const projection = snapshot.projection;
  const activeRuns = projection.active_runs ?? [];
  const pendingApprovals = projection.pending_approvals ?? [];
  const approvals: TranscriptItem[] = pendingApprovals.map((approval) => ({
    id: `approval-snapshot-${approval.approval_id}`,
    type: "approval",
    text: describeAction(approval.action),
    approvalId: approval.approval_id,
    approvalAction: approval.action,
    approvalRisk: approval.risk,
    approvalPattern: approval.pattern ?? undefined,
    timestamp: "",
  }));
  return {
    ...state,
    sessionAttachmentConfirmed: true,
    activeSessionId: sessionId,
    activeRunId: activeRuns.at(-1) ?? null,
    isRunning: activeRuns.length > 0,
    // `SessionProjection` carries `active_runs` but NOT their lifecycle state
    // (`crates/protocol/src/catchup.rs`), so a client that caught up this way
    // genuinely does not know whether the run is pausable. It says so — the
    // pause and resume buttons stay hidden until a `RunStateChanged` arrives.
    runState: null,
    // Likewise the snapshot says nothing about what the run is doing, only
    // that it is live: the working row shows until an event says more.
    activity: activeRuns.length > 0 ? { kind: "thinking" } : IDLE,
    // The queue, by contrast, IS in the snapshot, so it is known.
    pendingPrompts: projection.pending_prompts ?? [],
    transcript: [...state.transcript.filter((item) => item.type !== "approval"), ...approvals],
    // The projection is authoritative through this sequence even before the
    // paged history behind it arrives. Live continuity must start here or a
    // jump during that history read is mistaken for a harmless first event.
    lastSequence: Math.max(state.lastSequence, snapshot.through),
  };
}

function isProjectionSnapshot(snapshot: Catchup): snapshot is Extract<Catchup, { type: "Snapshot" }> {
  return snapshot.type === "Snapshot" && Boolean(snapshot.projection);
}

function mergeDurableEvent(state: DaemonState, event: SessionEvent): DaemonState {
  // Invariant: the daemon delivers a session's events in non-decreasing
  // `sequence` order, and `lastSequence` holds the highest sequence covered by
  // a snapshot/history watermark or retained live event. The live path is
  // therefore a plain append — no dedup scan, no Map rebuild, no re-sort per
  // event (those made every streamed token O(n)). Only genuinely out-of-order
  // input falls back to the full merge below.
  if (event.sequence > state.lastSequence) {
    let pendingGap = state.pendingGap;
    if (
      state.activeSessionId !== null &&
      state.lastSequence > 0 &&
      event.sequence > state.lastSequence + 1
    ) {
      // A gap means events this client never saw. Record the range so the
      // shell reads it back from the durable log — detecting it and moving on
      // left the transcript short by exactly these events, with nothing
      // marking the hole. An unrepaired earlier gap keeps its own lower bound
      // so overtaking it cannot lose it.
      pendingGap = {
        sessionId: state.activeSessionId,
        after: pendingGap ? Math.min(pendingGap.after, state.lastSequence) : state.lastSequence,
        through: event.sequence - 1,
      };
    }
    return {
      ...applyEvent(state, event),
      durableEvents: [...state.durableEvents, event],
      lastSequence: event.sequence,
      pendingGap,
    };
  }
  // A duplicate is found by BINARY SEARCH, not by rebuilding the world.
  //
  // `durableEvents` is non-decreasing in sequence — the fast path appends only
  // above the retained maximum, and every rebuild sorts — so membership is a
  // O(log n) question. It used to be answered by `mergeEvents`, which builds a
  // Map of the entire history and re-sorts it, O(n log n) for every single
  // event. Catch-up after a reconnect replays the session one event per frame
  // and every one of those is a duplicate, so a long session locked the UI for
  // seconds at precisely the moment the operator had finished waiting out the
  // reconnect backoff.
  if (retainsSequence(state.durableEvents, event.sequence)) {
    return state;
  }
  return rebuildFromEvents(state, mergeEvents(state.durableEvents, [event]));
}

/** Whether `events` — ordered by sequence — already holds `sequence`. */
function retainsSequence(events: readonly SessionEvent[], sequence: number): boolean {
  let low = 0;
  let high = events.length - 1;
  while (low <= high) {
    const mid = (low + high) >>> 1;
    const at = events[mid].sequence;
    if (at === sequence) {
      return true;
    }
    if (at < sequence) {
      low = mid + 1;
    } else {
      high = mid - 1;
    }
  }
  return false;
}

function mergeEvents(
  current: SessionEvent[],
  incoming: SessionEvent[],
): SessionEvent[] {
  const bySequence = new Map(current.map((event) => [event.sequence, event]));
  for (const event of incoming) {
    bySequence.set(event.sequence, event);
  }
  return [...bySequence.values()].sort((left, right) => left.sequence - right.sequence);
}

function rebuildFromEvents(state: DaemonState, events: SessionEvent[]): DaemonState {
  let projected: DaemonState = {
    ...state,
    activeRunId: null,
    isRunning: false,
    transcript: [],
    durableEvents: events,
    lastSequence: 0,
    // A rebuild replays the stream from the start, so the run state and the
    // queue must be re-derived from it rather than carried over from the
    // projection being replaced.
    runState: null,
    pendingPrompts: [],
    activity: IDLE,
    usage: null,
    activeObjective: null,
    objectivesByRun: {},
  };
  for (const event of events) {
    projected = applyEvent(projected, event, true);
  }
  return {
    ...projected,
    durableEvents: events,
    lastSequence: events.at(-1)?.sequence ?? 0,
  };
}

// A session can hold several concurrent runs — `SessionProjection.active_runs`
// is a `Vec<RunId>` (crates/protocol/src/catchup.rs) — and every run event
// names the run it belongs to. An event for a run this client is not showing
// must not move the run it IS showing: not its state, not its identity, not
// `isRunning`. When nothing is on screen the event is NOT foreign, so it gets
// adopted rather than dropped and attaching mid-run still lands on a run the
// operator can actually address.
function isForeignRun(state: DaemonState, runId: string): boolean {
  return Boolean(runId) && Boolean(state.activeRunId) && runId !== state.activeRunId;
}

/** How a resolved question is reported, from the outcome the daemon sent. */
function questionOutcomeText(outcome: unknown): string {
  const kind = (outcome as { type?: string } | undefined)?.type;
  const feedback = (outcome as { feedback?: string } | undefined)?.feedback;
  switch (kind) {
    case "Answered":
      return "Question answered";
    case "Rejected":
      return feedback ? `Question rejected: ${feedback}` : "Question rejected";
    case "Cancelled":
      return "Question cancelled";
    case "Expired":
      return "Question expired";
    default:
      // A newer daemon's outcome is reported as itself rather than guessed at.
      return kind ? `Question resolved: ${kind}` : "Question resolved";
  }
}

/** Drop one finished run's staged objective, leaving every sibling's. */
function forgetRunObjective(
  objectives: Record<string, string>,
  runId: string,
): Record<string, string> {
  if (!runId || !(runId in objectives)) {
    return objectives;
  }
  const remaining = { ...objectives };
  delete remaining[runId];
  return remaining;
}

// Bulk replay owns its initially empty transcript. It can append in place until
// the completed projection is published; live events must still copy the array
// so React and callers retain their previous immutable state.
function appendTranscript(items: TranscriptItem[], item: TranscriptItem, replay: boolean): TranscriptItem[] {
  if (!replay) return [...items, item];
  items.push(item);
  return items;
}

function replaceTranscriptItem(items: TranscriptItem[], index: number, item: TranscriptItem, replay: boolean): TranscriptItem[] {
  const next = replay ? items : [...items];
  next[index] = item;
  return next;
}

function applyEvent(state: DaemonState, event: SessionEvent, replay = false): DaemonState {
  const body = event.body;
  const at = event.occurred_at;
  const key = `${event.sequence}`;

  switch (body.type) {
    case "RunStarted": {
      const objective = asText(body.objective);
      const startedRunId = asText(body.run_id);
      // A sibling run starting must not hijack the surface. Last-run-wins here
      // would repoint `activeRunId` at a run the operator was not watching and
      // immediately offer pause for it. The objective still joins the
      // transcript — it is session history either way.
      // Remembered for EVERY run, the siblings included: a sibling's failure
      // card still needs its own objective, and by the time it arrives the
      // surface may have adopted it.
      const objectivesByRun =
        startedRunId && objective
          ? { ...state.objectivesByRun, [startedRunId]: objective }
          : state.objectivesByRun;
      if (isForeignRun(state, startedRunId)) {
        return {
          ...state,
          objectivesByRun,
          transcript: appendTranscript(state.transcript,
            { id: `user-${key}`, type: "user", text: objective, timestamp: at }, replay),
        };
      }
      return {
        ...state,
        activeRunId: startedRunId || state.activeRunId,
        isRunning: true,
        // `StartRun` inserts the run row in the SAME transaction as this event,
        // with `RunState::Queued` (`ProjectionOp::InsertRun` ->
        // `projections::insert_run`, which binds `run_state_to_db(Queued)`).
        // So this is the daemon's state, read off its own contract, not a
        // guess — and `Queued` is a state `validate_run_transition` admits a
        // pause from. `Preparing`/`Running` follow as their own events.
        runState: "Queued",
        activity: { kind: "thinking" },
        activeObjective: objective || state.activeObjective,
        objectivesByRun,
        usage: null,
        transcript: appendTranscript(state.transcript,
          { id: `user-${key}`, type: "user", text: objective, timestamp: at }, replay),
      };
    }

    case "ModelStreamDelta": {
      const text = asText(body.text);
      if (!text) {
        return state;
      }
      const runId = asText(body.run_id);
      // Keep provider progress separate from the answer.
      const kind = body.thought === true ? "thought" : "assistant";
      const prefix = `${kind}-${runId}-`;
      const activity: RunActivity = isForeignRun(state, runId)
        ? state.activity
        : { kind: "streaming" };
      const last = state.transcript[state.transcript.length - 1];
      if (last && last.type === kind && last.id.startsWith(prefix)) {
        const merged: TranscriptItem = { ...last, text: last.text + text };
        return { ...state, activity, transcript: replaceTranscriptItem(state.transcript, state.transcript.length - 1, merged, replay) };
      }
      return {
        ...state,
        activity,
        transcript: appendTranscript(state.transcript,
          { id: `${prefix}${key}`, type: kind, text, runId, timestamp: at }, replay),
      };
    }

    case "ToolStarted": {
      const tool = asText(body.tool);
      const runId = asText(body.run_id);
      const label = asText(body.label);
      const acp = tool.startsWith("acp:");
      const separator = acp ? label.indexOf(" · ") : -1;
      const toolKind = separator >= 0 ? label.slice(0, separator) : (acp ? label || "Tool" : tool);
      const toolLabel = separator >= 0 ? label.slice(separator + 3) : label || toolKind;
      const invocation: TranscriptItem = {
        id: `tool-${key}`,
        type: "tool_call",
        text: toolLabel,
        toolName: tool,
        toolRunId: runId,
        toolKind,
        toolLabel,
        status: "running",
        timestamp: at,
      };
      // ACP sends snapshots as its arguments/title stream. The stable call
      // identity means a refinement changes one card, including after replay.
      let transcript = state.transcript;
      const existing = acp ? transcript.findIndex((item) => item.type === "tool_call"
        && item.toolName === tool && item.toolRunId === runId) : -1;
      if (existing >= 0) {
        const previous = transcript[existing];
        transcript = replaceTranscriptItem(transcript, existing, {
          ...previous, text: toolLabel, toolLabel, toolKind,
        }, replay);
      } else {
        transcript = appendTranscript(transcript, invocation, replay);
      }
      return {
        ...state,
        activity: isForeignRun(state, runId) || (existing >= 0 && transcript[existing].status !== "running")
          ? state.activity : { kind: "tool", tool: acp ? toolLabel : tool },
        transcript,
      };
    }

    case "ToolCompleted": {
      const tool = asText(body.tool);
      const runId = asText(body.run_id);
      const outcome = body.outcome as { type?: string; message?: string } | undefined;
      const status: TranscriptItem["status"] = outcome?.type === "Succeeded" ? "success" : "error";
      // The wire deliberately carries no tool output (bulk output is an
      // artifact) — but a FAILURE carries its message, and dropping it left
      // a red "error" with no reason anywhere in the app.
      const failureReason =
        status === "error" && typeof outcome?.message === "string" && outcome.message
          ? outcome.message
          : undefined;
      const artifactId = ("artifact" in body && body.artifact && typeof body.artifact === "object" && "id" in body.artifact)
        ? String(body.artifact.id)
        : undefined;
      // Native tools match the latest running call by name within their run.
      // ACP machine names include the invocation id, so concurrent calls with
      // the same human title resolve correctly even when they finish out of order.
      let transcript = state.transcript;
      for (let index = transcript.length - 1; index >= 0; index--) {
        const item = transcript[index];
        if (item.type === "tool_call" && item.toolName === tool && item.toolRunId === runId && item.status === "running") {
          transcript = replaceTranscriptItem(transcript, index, {
            ...item,
            status,
            ...(artifactId ? { artifactId, artifact: body.artifact as ArtifactRef } : {}),
            ...(failureReason ? { toolResult: failureReason } : {}),
          }, replay);
          break;
        }
      }
      return {
        ...state,
        // The tool returned; until the next token or tool the model is
        // deliberating, which is what the working row says.
        activity: isForeignRun(state, runId) ? state.activity : afterToolActivity({ ...state, transcript }),
        transcript,
      };
    }

    case "ToolUpdated": {
      const tool = asText(body.tool);
      const runId = asText(body.run_id);
      const label = asText(body.label);
      if (!label) return state;
      const index = state.transcript.findIndex((item) => item.type === "tool_call"
        && item.toolName === tool && item.toolRunId === runId);
      if (index < 0) return state;
      const separator = tool.startsWith("acp:") ? label.indexOf(" · ") : -1;
      const toolKind = separator >= 0 ? label.slice(0, separator) : (tool.startsWith("acp:") ? label : tool);
      const toolLabel = separator >= 0 ? label.slice(separator + 3) : label;
      const item = state.transcript[index];
      const transcript = replaceTranscriptItem(state.transcript, index, {
        ...item, text: toolLabel, toolKind, toolLabel,
      }, replay);
      return {
        ...state, transcript,
        activity: !isForeignRun(state, runId) && item.status === "running"
          && state.activity.kind === "tool"
          && (state.activity.tool === item.toolLabel || state.activity.tool === item.toolName)
          ? { kind: "tool", tool: toolLabel } : state.activity,
      };
    }

    case "ModelRetrying": {
      // The provider refused (overloaded, rate-limited, a transient network
      // fault) and the daemon is backing off before trying again
      // (`EventBody::ModelRetrying`). Dropped, a four-attempt backoff looked
      // like a hang: static transcript, "Run in progress…", nothing moving.
      const attempt = typeof body.attempt === "number" ? body.attempt : 0;
      const maxAttempts = typeof body.max_attempts === "number" ? body.max_attempts : 0;
      // Sanitised like any other provider text. This message is the reason a
      // request failed, and a 500 from a proxy can echo the request's own
      // `Authorization` header — so the credential was on screen for the whole
      // backoff, and durably in the transcript, even though the eventual
      // failure card scrubs the same chain.
      const message =
        sanitizeFailureText(asText(body.message)) || "the provider request failed";
      const delayMs = typeof body.delay_ms === "number" ? body.delay_ms : 0;
      const seconds = Math.max(1, Math.round(delayMs / 1000));
      const foreign = isForeignRun(state, asText(body.run_id));
      return {
        ...state,
        activity: foreign
          ? state.activity
          : { kind: "retrying", attempt, maxAttempts, message, delayMs },
        transcript: appendTranscript(state.transcript,
          {
            id: `retry-${key}`,
            type: "system",
            tone: "info",
            text: `Retrying (${attempt}/${maxAttempts}): ${message} — next attempt in ${seconds}s`,
            timestamp: at,
          }, replay),
      };
    }

    case "ToolDenied": {
      // Policy refused a proposed action before it ran. Without this row the
      // agent's request simply vanished, and a run that was blocked by policy
      // read as one that had nothing to do.
      const reasons =
        "reasons" in body && Array.isArray(body.reasons)
          ? body.reasons.filter((reason): reason is string => typeof reason === "string")
          : [];
      return {
        ...state,
        transcript: appendTranscript(state.transcript,
          {
            id: `denied-${key}`,
            type: "system",
            runId: asText(body.run_id),
            tone: "warning",
            text: `Blocked by policy: ${describeAction(body.action)}${
              reasons.length > 0 ? ` — ${reasons.join("; ")}` : ""
            }`,
            timestamp: at,
          }, replay),
      };
    }

    case "BudgetWarning": {
      const dimension = (body.dimension as { type?: string } | undefined)?.type ?? "budget";
      const used = typeof body.used === "number" ? body.used : 0;
      const limit = typeof body.limit === "number" ? body.limit : 0;
      if (dimension === "Tokens" && limit > 0 && used / limit < 0.8) return state;
      const id = `budget-${asText(body.run_id)}-${dimension}`;
      const item: TranscriptItem = {
        id, type: "system", tone: "warning", timestamp: at,
        text: `Budget warning: ${budgetLabel(dimension)} ${used}/${limit}`,
      };
      const index = state.transcript.findIndex((candidate) => candidate.id === id);
      return {
        ...state,
        transcript: index < 0 ? appendTranscript(state.transcript, item, replay)
          : replaceTranscriptItem(state.transcript, index, item, replay),
      };
    }

    case "RunUsage": {
      const runId = asText(body.run_id);
      if (isForeignRun(state, runId)) {
        return state;
      }
      return {
        ...state,
        usage: {
          runId,
          promptTokens: typeof body.prompt_tokens === "number" ? body.prompt_tokens : null,
          completionTokens:
            typeof body.completion_tokens === "number" ? body.completion_tokens : null,
          costMicros: typeof body.cost_micros === "number" ? body.cost_micros : null,
          coverage: body.coverage ?? null,
        },
      };
    }

    case "ApprovalRequested":
      return {
        ...state,
        activity: state.isRunning ? { kind: "waiting", on: "approval" } : state.activity,
        transcript: appendTranscript(state.transcript,
          {
            id: `approval-${key}`,
            type: "approval",
            text: describeAction(body.action),
            approvalId: asText(body.approval_id),
            approvalAction: body.action,
            approvalRisk: body.risk,
            approvalPattern: body.pattern ?? undefined,
            decisionRevision: event.sequence,
            timestamp: at,
          }, replay),
        // The inbox badge moves the moment attention is needed — the entry
        // list itself still refreshes from the daemon (connect / Refresh),
        // which replaces this running estimate with the true count.
        unreadInboxCount: state.unreadInboxCount + 1,
      };

    case "ApprovalResolved": {
      const approvalId = asText(body.approval_id);
      const decrement = stillCountsAsUnread(state, { type: "Approval", id: approvalId });
      return {
        ...state,
        activity: state.activity.kind === "waiting" ? { kind: "thinking" } : state.activity,
        transcript: state.transcript.filter((item) => item.approvalId !== approvalId),
        unreadInboxCount: decrement ? Math.max(0, state.unreadInboxCount - 1) : state.unreadInboxCount,
      };
    }

    case "QuestionAsked": {
      const questions = "questions" in body && Array.isArray(body.questions) ? body.questions : [];
      const question = questions[0] as { header?: string; question?: string } | undefined;
      const text = question ? `${question.header ?? ""}: ${question.question ?? ""}` : "Question prompted";
      const prompts = questions.map(normaliseQuestionPrompt);
      // The card is keyed by the daemon's question id so `QuestionResolved` can
      // retire exactly it — which means a RE-ISSUED question would append a
      // second card under a key React already has. Duplicate keys stack the
      // cards and leave reconciliation undefined, so a re-issue replaces the
      // card in place, exactly as the TUI replaces the pending question.
      const cardId = `question-${asText(body.question_id) || key}`;
      const card: TranscriptItem = {
        id: cardId,
        type: "question",
        text,
        timestamp: at,
        questionId: asText(body.question_id),
        decisionRevision: event.sequence,
        questionRunId: asText(body.run_id),
        questionPrompts: prompts,
      };
      const existingCard = state.transcript.findIndex((item) => item.id === cardId);
      // A SIBLING's question is still a card worth showing — it is session
      // history, and it is answerable — but it must not claim the displayed
      // run is waiting. Run A can be streaming while run B asks something, and
      // saying "waiting for your answer" about A is simply false.
      const waiting: RunActivity =
        state.isRunning && !isForeignRun(state, asText(body.run_id))
          ? { kind: "waiting", on: "question" }
          : state.activity;
      if (existingCard !== -1) {
        const transcript = replaceTranscriptItem(state.transcript, existingCard, card, replay);
        return { ...state, activity: waiting, transcript };
      }
      return {
        ...state,
        activity: waiting,
        transcript: appendTranscript(state.transcript, card, replay),
        // A NEW question needs attention (a re-issue replaced its card above
        // without reaching here, so it never double-counts).
        unreadInboxCount: state.unreadInboxCount + 1,
      };
    }

    case "QuestionResolved": {
      // Mirror `ApprovalResolved`: a resolved question is no longer actionable,
      // so its card leaves the transcript and only the note stays. Matching is
      // by the question id `QuestionAsked` carried, never by position or text.
      const questionId = asText(body.question_id);
      const decrement = stillCountsAsUnread(state, { type: "Question", id: questionId });
      // `QuestionResolved` carries no run id, so the answer comes from the
      // card the ASK left behind. Without it, resolving a sibling's question
      // moved the displayed run out of a waiting state it was still in — for
      // its OWN approval or question.
      const askedBy = state.transcript.find(
        (item) => item.type === "question" && item.id === `question-${questionId}`,
      )?.questionRunId;
      const resolvedForeign = Boolean(askedBy) && isForeignRun(state, askedBy ?? "");
      return {
        ...state,
        activity:
          state.activity.kind === "waiting" && !resolvedForeign
            ? { kind: "thinking" }
            : state.activity,
        unreadInboxCount: decrement ? Math.max(0, state.unreadInboxCount - 1) : state.unreadInboxCount,
        transcript: [
          ...state.transcript.filter(
            (item) => item.type !== "question" || item.id !== `question-${questionId}`,
          ),
          {
            id: `question-resolved-${key}`,
            type: "system",
            // The outcome was discarded, so a REJECTED question read
            // "Question answered" — telling the operator the opposite of what
            // happened to their own decision.
            text: questionOutcomeText(body.outcome),
            timestamp: at,
          },
        ],
      };
    }

    case "PatchProposed": {
      const artifactId = ("artifact" in body && body.artifact && typeof body.artifact === "object" && "id" in body.artifact)
        ? String(body.artifact.id)
        : "";
      // The wire carries the touched paths, ± line counts, and a bounded
      // diff preview; printing only the artifact id threw all of it away and
      // left the actual change unreviewable anywhere in this app.
      const files =
        "files" in body && Array.isArray(body.files)
          ? body.files.filter((file): file is string => typeof file === "string")
          : [];
      const additions =
        "additions" in body && typeof body.additions === "number" ? body.additions : 0;
      const deletions =
        "deletions" in body && typeof body.deletions === "number" ? body.deletions : 0;
      // The wire field is `preview` (`EventBody::PatchProposed`). Reading
      // `diff_preview` matched nothing, so the patch card's diff was always
      // empty — a field name invented rather than read off the protocol.
      const diffPreview =
        "preview" in body && typeof body.preview === "string" && body.preview
          ? body.preview
          : undefined;
      const summary =
        files.length > 0
          ? `Patch proposed: ${files.length} file${files.length === 1 ? "" : "s"} (+${additions} −${deletions})`
          : `Patch proposed: artifact ${artifactId}`;
      return {
        ...state,
        transcript: appendTranscript(state.transcript,
          {
            id: `patch-${key}`,
            type: "system",
            text: summary,
            artifactId,
            artifact: body.artifact,
            diffPreview,
            patchFiles: files,
            patchRunId: asText(body.run_id) || undefined,
            patchAdditions: additions,
            patchDeletions: deletions,
            timestamp: at,
          }, replay),
      };
    }

    case "NoteAppended": {
      const text = asText(body.text);
      if (!text) {
        return state;
      }
      // Backstage fold, ported from the TUI (`reduce.rs`, `TranscriptEntry::
      // Backstage`). The context manifest and curated-memory writes are real
      // but are NOT part of the visible conversation, and the daemon labels
      // both by the note's own prefix — context by
      // `knowledge/src/context.rs`'s `=== CONTEXT` header, memory by
      // `executor.rs`'s `remembered: {statement}`. Printed inline they bury
      // the actual answer under a screenful of tool manifest, which is exactly
      // what this client was doing.
      const isContext = text.startsWith("=== CONTEXT");
      const isMemory = text.trimStart().startsWith("remembered:");
      if (isContext || isMemory) {
        // Per RUN, not per session — the comment above says "one per run" and
        // the lookup did not, so a second run's manifest folded into the first
        // run's row: counters misattributed across runs, and `raw` grew without
        // bound as every run's full tool manifest was appended to one card.
        const backstageId = `backstage-${asText(body.run_id) || "session"}`;
        const at_index = state.transcript.findIndex(
          (item) => item.type === "backstage" && item.id === backstageId,
        );
        if (at_index === -1) {
          return {
            ...state,
            transcript: appendTranscript(state.transcript,
              {
                id: backstageId,
                type: "backstage",
                text: "",
                timestamp: at,
                contextLines: isContext ? text.split("\n").length : undefined,
                memoryUpdates: isMemory ? 1 : 0,
                raw: [text],
              }, replay),
          };
        }
        // Find-or-update: at most ONE backstage row per run, however many
        // manifests and memory writes arrive.
        const existing = state.transcript[at_index];
        const merged: TranscriptItem = {
          ...existing,
          contextLines: isContext ? text.split("\n").length : existing.contextLines,
          memoryUpdates: (existing.memoryUpdates ?? 0) + (isMemory ? 1 : 0),
          raw: [...(existing.raw ?? []), text],
        };
        const transcript = replaceTranscriptItem(state.transcript, at_index, merged, replay);
        return { ...state, transcript };
      }
      return {
        ...state,
        transcript: appendTranscript(state.transcript,
          { id: `note-${key}`, type: "system", text, timestamp: at }, replay),
      };
    }

    case "RunCompleted": {
      const disposition = body.disposition as
        | { type?: string; reason?: string; summary?: string; error?: StructuredFailure }
        | undefined;
      const kind = disposition?.type ?? "Unknown";
      const reason = disposition?.reason ?? disposition?.summary;
      // `RunCompleted` carries `run_id` (crates/protocol/src/events.rs). A
      // sibling run finishing must not clear the run on screen — that wipes a
      // live run out of the UI entirely, the most destructive sibling leak of
      // the three.
      const completedRunId = asText(body.run_id);
      const completedForeign = isForeignRun(state, completedRunId);
      const settled = settleRunTools(state.transcript, completedRunId, replay);
      // A SUCCESSFUL run adds no row. The streamed model prose already ended
      // the turn, so "Run completed: <summary>" printed the same answer a
      // second time — visible in the transcript as the reply, then the same
      // words again on a dim centred line. The TUI settled this already
      // (`render.rs`, `TranscriptEntry::Completed`): "the streamed model prose
      // already ended the turn — render nothing here". Failures and
      // cancellations still announce themselves, because nothing else does.
      let appended: TranscriptItem | null = null;
      if (kind === "Failed") {
        // A failure is its own card, with the reason sanitised and a next
        // step attached. It used to be a dim centred system row — and past
        // 160 characters, a folded one — so the single most important
        // message in the session was the least visible thing on screen.
        const diagnosis = diagnoseFailure(reason ?? "", disposition?.error);
        appended = {
          id: `run-${key}`,
          type: "failure",
          runId: completedRunId,
          text: diagnosis.summary,
          failureDetail: diagnosis.detail,
          // THIS run's objective, not whichever one the surface is showing.
          // A sibling that started while another run was displayed was never
          // staged in `activeObjective`, and by the time it fails the surface
          // may have adopted it — so Retry would have resubmitted the older
          // run's work, and paid for it.
          objective:
            (completedRunId ? state.objectivesByRun[completedRunId] : undefined) ??
            (completedForeign ? undefined : (state.activeObjective ?? undefined)),
          remedy: diagnosis.remedy,
          hint: diagnosis.hint ?? undefined,
          timestamp: at,
        };
      } else if (kind !== "Completed") {
        appended = {
          id: `run-${key}`,
          type: "system",
          text: reason ? `Run ${kind.toLowerCase()}: ${reason}` : `Run ${kind.toLowerCase()}`,
          timestamp: at,
        };
      }
      return {
        ...state,
        isRunning: completedForeign ? state.isRunning : false,
        activeRunId: completedForeign ? state.activeRunId : null,
        runState: completedForeign ? state.runState : null,
        activity: completedForeign ? state.activity : IDLE,
        transcript: appended ? appendTranscript(settled, appended, replay) : settled,
        // The run is over; its objective has been read into the card above and
        // the map must not grow for the life of the session.
        objectivesByRun: forgetRunObjective(state.objectivesByRun, completedRunId),
      };
    }

    case "RunStateChanged": {
      const runState = (body.state as { type?: string } | undefined)?.type ?? "";
      // The daemon names the run this transition belongs to. A session can hold
      // several runs, so a transition for a run this client is not showing must
      // not move the state of the one it is.
      const eventRunId = asText(body.run_id);
      const terminal = ["Completed", "Failed", "Cancelled"].includes(runState);
      const settled = terminal ? settleRunTools(state.transcript, eventRunId, replay) : state.transcript;
      if (isForeignRun(state, eventRunId)) {
        return settled === state.transcript ? state : { ...state, transcript: settled };
      }
      if (terminal) {
        return { ...state, transcript: settled, isRunning: false, activeRunId: null, runState: null, activity: IDLE };
      }
      // Adopt the run this transition names when nothing is on screen.
      // Recording `isRunning: true` against a null `activeRunId` would leave
      // the surface claiming a run is live while holding no id to address it
      // with — no pause, no cancel, no way back.
      const adoptedRunId = state.activeRunId ?? (eventRunId || null);
      // Everything else is a live state and is recorded VERBATIM, including
      // `Paused` and any tag a newer daemon invents. The pause/resume controls
      // read this field; an unrecognised tag simply matches neither, so a newer
      // state disables both buttons instead of mislabelling one.
      return {
        ...state,
        activeRunId: adoptedRunId,
        isRunning: runState === "Running" ? true : state.isRunning,
        runState: (runState || null) as RunState["type"] | null,
        activity:
          runState === "WaitingForApproval"
            ? { kind: "waiting", on: "approval" }
            : runState === "WaitingForUserInput"
              ? { kind: "waiting", on: "question" }
              : runState === "Paused"
                ? IDLE
                : state.activity.kind === "idle" && runState === "Running"
                  ? { kind: "thinking" }
                  : state.activity,
      };
    }

    // The WHOLE queue after a mutation, latest-wins: fold by REPLACING, so a
    // replay of history converges on the daemon's final queue rather than an
    // accumulation of every queue it ever had.
    case "PendingPromptsChanged":
      return {
        ...state,
        pendingPrompts: Array.isArray(body.prompts) ? [...body.prompts] : [],
      };

    // Every other event kind — including one a newer daemon invented — is
    // carried by the stream but not rendered. Silence is correct here;
    // inventing a card for an event this client cannot read is not.
    default:
      return state;
  }
}

function asText(value: unknown): string {
  return typeof value === "string" ? value : "";
}

const IDLE: RunActivity = { kind: "idle" };

/** A wire `QuestionPrompt` with its serde defaults applied, never trusted blindly. */
function normaliseQuestionPrompt(raw: unknown): QuestionPromptView {
  const record = (raw && typeof raw === "object" ? raw : {}) as Record<string, unknown>;
  const options = Array.isArray(record.options)
    ? record.options.flatMap((option) => {
        const entry = (option && typeof option === "object" ? option : {}) as Record<string, unknown>;
        const label = asText(entry.label);
        if (!label) {
          return [];
        }
        const description = asText(entry.description);
        return [description ? { label, description } : { label }];
      })
    : [];
  return {
    header: asText(record.header),
    question: asText(record.question),
    options,
    multiple: record.multiple === true,
    // `custom` defaults to TRUE on the wire (`crates/protocol/src/question.rs`):
    // only an explicit false disables the typed answer.
    custom: record.custom !== false,
  };
}

/** After a tool returns: still waiting on a human if we were, else deliberating. */
function afterToolActivity(state: DaemonState): RunActivity {
  if (state.activity.kind === "waiting") return state.activity;
  for (let index = state.transcript.length - 1; index >= 0; index--) {
    const item = state.transcript[index];
    if (item.type === "tool_call" && item.status === "running" && item.toolRunId === state.activeRunId) {
      return { kind: "tool", tool: item.toolLabel || item.toolName || "Tool" };
    }
  }
  return { kind: "thinking" };
}

/** A final run has no running tools. Lost older ACP identities cannot tell us
 * which outcome belongs to which card, so finish neutrally rather than guess. */
function settleRunTools(items: TranscriptItem[], runId: string, replay: boolean): TranscriptItem[] {
  let next = items;
  for (let index = 0; index < next.length; index++) {
    const item = next[index];
    if (item.type === "tool_call" && item.toolRunId === runId && item.status === "running") {
      if (next === items && !replay) next = [...items];
      next[index] = { ...item, status: "finished" };
    }
  }
  return next;
}

/** The `BudgetDimension` tag as the TUI labels it (`render.rs::budget_label`). */
function budgetLabel(dimension: string): string {
  switch (dimension) {
    case "Tokens":
      return "tokens";
    case "Cost":
      return "cost";
    case "WallClock":
      return "wall-clock";
    case "ToolCalls":
      return "tool-calls";
    default:
      return "budget";
  }
}

/**
 * Whether a resolved approval/question should still subtract from
 * `unreadInboxCount`, or whether the authoritative `inbox` array already
 * retired it — via `inbox-entry-updated`, when the user acted on it through
 * the Inbox view before this broadcast arrived. Subtracting again there
 * would remove an UNRELATED still-unread entry instead, since the count by
 * then no longer has this one to give up.
 */
function stillCountsAsUnread(
  state: DaemonState,
  identity: { type: "Approval"; id: string } | { type: "Question"; id: string },
): boolean {
  if (state.inboxStatus !== "loaded") {
    // Nothing authoritative to reconcile against yet; the running estimate
    // owns this count alone.
    return true;
  }
  const entry = state.inbox.find((candidate) => {
    const sourceIdentity = candidate.source.identity;
    return identity.type === "Approval"
      ? sourceIdentity.type === "Approval" && sourceIdentity.approval_id === identity.id
      : sourceIdentity.type === "Question" && sourceIdentity.question_id === identity.id;
  });
  return !entry || !entry.state || entry.state.type === "Unread";
}

function describeAction(action: unknown): string {
  if (action && typeof action === "object") {
    const record = action as Record<string, unknown>;
    const kind = asText(record.type) || "action";
    const programStr = record.program
      ? `${record.program} ${Array.isArray(record.args) ? record.args.join(" ") : ""}`.trim()
      : "";
    const detail =
      programStr
      || asText(record.command)
      || asText(record.path)
      || asText(record.summary)
      || asText(record.url);
    return detail ? `${kind}: ${detail}` : kind;
  }
  return "action";
}
