/**
 * Connects the store in `daemonState.ts` to the transport in `transport.ts`.
 *
 * The connection is attempted on mount, and retried with backoff whenever the
 * store reports the socket dropped. It succeeds only when the shell's
 * handshake with `codypendentd` succeeded; every other outcome — no shell, no
 * socket, no daemon — lands in a disconnected state carrying the reason.
 * Submit and cancel go straight to the daemon and do nothing locally on
 * failure beyond reporting it.
 */
import { useCallback, useEffect, useReducer, useRef, useState } from "react";

import type {
  AnalyticsExportRequest,
  AnalyticsExportResult,
  AnalyticsPage,
  AnalyticsQuery,
  ArtifactRef,
  InboxListQuery,
  PromptDelivery,
} from "@codypendent/protocol";
import {
  createTransport,
  type ApprovalChoice,
  type DaemonFrame,
  type DesktopTransport,
  type SessionRow,
} from "./transport.js";
import { initialState, reduce, type DaemonState } from "./daemonState.js";
import type { QuestionOutcomeView } from "./types.js";
import type { ApprovalGrant } from "./approval.js";
import { publishFrame } from "./frameBus.js";
import {
  BlockingWorkNotifier,
  defaultNotificationSink,
  type NotificationSink,
} from "./osNotifications.js";

/** Shown when the UI is running outside the Tauri shell (a browser tab). */
export const NO_SHELL_DETAIL =
  "Not running in the Codypendent desktop shell, so there is no daemon transport. Start it with `npm run tauri:dev`.";

/** Reconnect backoff after a dropped socket: 1s, 2s, 5s, then every 15s. */
const RETRY_DELAYS_MS = [1000, 2000, 5000, 15000];

type PendingAttachment = {
  generation: number;
  sessionId: string;
  frames: DaemonFrame[];
};

/**
 * What actually happened to a steering send, reported back to the caller.
 *
 * `accepted` is the daemon's acceptance of the `QueueSteering` command and
 * nothing more. Whether the steering was QUEUED, and whether it was later
 * APPLIED, arrive separately as `SteeringQueued` / `SteeringApplied` events —
 * the steering panel reads those from the durable event stream and never
 * upgrades an acceptance into either of them.
 */
export type SteerOutcome =
  | { accepted: true; at: string }
  | { accepted: false; detail: string };

export interface DaemonController {
  state: DaemonState;
  /**
   * Drop the connection and open a fresh one.
   *
   * The repository is fixed at connect, so rebinding to a different checkout
   * needs a new connection — which is what `RepoPicker`'s long-declared but
   * never-wired `onReconnect` was for.
   */
  reconnect: () => Promise<void>;
  /**
   * Start `codypendentd` through the shell and reconnect the moment it
   * answers. Resolves with what happened, in words the banner can show;
   * `started` is false when the shell could not start one, and the detail then
   * names what was tried and the manual command.
   */
  startDaemon: () => Promise<{ started: boolean; detail: string }>;
  /** Clear the client-error banner (the operator read it). */
  dismissError: () => void;
  /**
   * Start a run. Resolves `true` when the daemon ACCEPTED the objective and
   * `false` when it (or the shell) refused; the refusal is also reported in
   * the error banner. The composer keeps the draft on `false`.
   */
  submit: (objective: string) => Promise<boolean>;
  /** Prepare a separate chat; keeps the previous durable session and draft. */
  newSession: () => boolean;
  cancel: () => Promise<void>;
  /** Queue steering text against the live run. See {@link SteerOutcome}. */
  steer: (text: string) => Promise<SteerOutcome>;
  /**
   * Send a real `PauseRun` for the live run — stop it without killing it.
   *
   * Callers must gate on {@link runLifecycleAffordance}: this does not check
   * whether the transition is legal, because the daemon is the authority on
   * that and a client-side second opinion would only ever drift from it.
   */
  pauseRun: () => Promise<void>;
  /** Send a real `ResumeRun` for the paused run. */
  resumeRun: () => Promise<void>;
  /**
   * Queue a follow-up prompt on the attached session (`QueuePrompt`).
   *
   * Resolves `true` when the daemon ACCEPTED the command. The queue itself
   * changes only when the daemon's `PendingPromptsChanged` event arrives; this
   * never writes an entry into the projection itself.
   */
  queuePrompt: (text: string, delivery?: PromptDelivery) => Promise<boolean>;
  /** Edit a queued prompt in place; absent fields keep their values. */
  updateQueuedPrompt: (
    promptId: string,
    text?: string | null,
    delivery?: PromptDelivery | null,
  ) => Promise<boolean>;
  /** Promote a queued prompt to `Steer` and move it to the front. */
  promoteQueuedPrompt: (promptId: string) => Promise<boolean>;
  /** Remove a queued prompt without running it. */
  deleteQueuedPrompt: (promptId: string) => Promise<boolean>;
  /** True only after the daemon confirms the attachment. */
  selectSession: (sessionId: string) => Promise<boolean>;
  resolveApproval: (approvalId: string, decision: ApprovalChoice, scope?: ApprovalGrant) => Promise<void>;
  /** Answer or reject a parked question; the caller presents refusal feedback. */
  resolveQuestion: (questionId: string, outcome: QuestionOutcomeView) => Promise<void>;
  loadInbox: (query?: InboxListQuery) => Promise<void>;
  acknowledgeInbox: (entryId: string) => Promise<void>;
  dismissInbox: (entryId: string) => Promise<void>;
  queryAnalytics: (query?: AnalyticsQuery) => Promise<AnalyticsPage | null>;
  exportAnalytics: (request: AnalyticsExportRequest) => Promise<AnalyticsExportResult | null>;
  readArtifact: (artifact: ArtifactRef) => Promise<Uint8Array | null>;
  /**
   * The live bridge, or `null` outside the shell.
   *
   * Exposed so the panels that model their own daemon-side resource — the
   * Session Library, workflow runs, the task board, a run's blackboard — can
   * call the bridge directly instead of routing every one of those reads
   * through a store that models a single session's transcript. It is the same
   * connected instance this hook handshook with; nothing else opens one.
   */
  transport: DesktopTransport | null;
}

export function useDaemon(
  makeTransport: () => DesktopTransport | null = createTransport,
  /**
   * Where blocking-work notifications go. Defaults to the OS, through Tauri's
   * notification plugin, whenever the app is running inside the shell; a test
   * injects its own sink to observe exactly what a user would be shown.
   */
  notify?: NotificationSink,
): DaemonController {
  // The first render precedes the transport effect. A null bridge here means
  // "not checked yet", not "browser preview"; only shell-missing can establish
  // that the native bridge is absent.
  const [state, dispatch] = useReducer(reduce, initialState, (initial): DaemonState => ({
    ...initial,
    status: "connecting",
    detail: "Checking the local workspace connection…",
  }));
  const factory = useRef(makeTransport);
  const transport = useRef<DesktopTransport | null>(null);
  const connectionGeneration = useRef(0);
  const connectionReady = useRef(false);
  const inboxReadGeneration = useRef(0);
  /**
   * The transport as render-visible state, mirrored from the ref: callbacks
   * read the ref (they are event handlers, so a stale read never matters),
   * but consumers rendering off `controller.transport` must re-render when it
   * appears, which a ref read during render would never trigger.
   */
  const [bridge, setBridge] = useState<DesktopTransport | null>(null);
  /**
   * Reconnect bookkeeping: how many attempts since the last success, and the
   * tick that re-runs the connect effect below for each retry.
   */
  const reconnectAttempts = useRef(0);
  const [reconnectTick, setReconnectTick] = useState(0);
  /**
   * The waiter `reconnect()` handed out, answered when the attempt it started
   * settles. A caller that reports "reconnected" needs the real outcome, not
   * the fact that a counter moved.
   */
  const pendingReconnect = useRef<{
    resolve: () => void;
    reject: (error: Error) => void;
  } | null>(null);

  /** Answer the pending waiter, if any. A no-op when nobody asked. */
  const settleReconnect = useCallback((error?: Error) => {
    const pending = pendingReconnect.current;
    if (!pending) {
      return;
    }
    pendingReconnect.current = null;
    if (error) {
      pending.reject(error);
    } else {
      pending.resolve();
    }
  }, []);
  /**
   * Attach replies replay their catch-up before `attachSession` resolves.
   * Hold that replay outside the committed projection until the command is
   * known to have succeeded; a refused attach discards it and leaves the last
   * confirmed session untouched.
   */
  const pendingAttachment = useRef<PendingAttachment | null>(null);
  const attachmentGeneration = useRef(0);
  /** Serializes rapid A -> B -> C choices so UI and daemon commit in one order. */
  const attachmentQueue = useRef<Promise<void> | null>(null);
  /** Session the current native connection has actually confirmed. */
  const confirmedAttachment = useRef<string | null>(null);
  /** A new chat waits for its first accepted run; old subscription frames cannot reopen it. */
  const freshSession = useRef(false);
  /**
   * True from the moment a launch is sent until the daemon has answered it.
   * `state.isRunning` only turns true on `RunStateChanged`, so it cannot serve
   * as this guard.
   */
  const launchInFlight = useRef(false);
  // A first objective creates a session before its reply names it. Retain
  // early frames until the accepted handle binds them; follow-ups keep the
  // current confirmed session and its history.
  const launchFrames = useRef<DaemonFrame[] | null>(null);
  /** The session a reconnect re-attaches; mirrors `state.activeSessionId`. */
  const activeSession = useRef<string | null>(null);
  activeSession.current = state.activeSessionId;
  /** Session titles, for naming the parked session in a notification. */
  const sessionTitles = useRef(new Map<string, string>());
  /**
   * Approvals and questions are the only two things that stop a run until a
   * human acts, so they are the only two that raise an OS notification. Every
   * other event stays in the transcript and the inbox badge
   * (`osNotifications.ts` explains why over-notifying is the bug).
   */
  const notifier = useRef<BlockingWorkNotifier | null>(null);
  if (notifier.current === null) {
    notifier.current = new BlockingWorkNotifier(
      notify ?? defaultNotificationSink((sessionId) =>
        sessionId === null ? undefined : sessionTitles.current.get(sessionId),
      ),
    );
  }

  const deliverFrame = useCallback((frame: DaemonFrame) => {
    if (freshSession.current && (frame.kind === "event" || frame.kind === "history" || frame.kind === "catchup" || frame.kind === "remote_ui")) return;
    if (frame.kind === "disconnected") {
      connectionReady.current = false;
      connectionGeneration.current += 1;
      confirmedAttachment.current = null;
      launchFrames.current = null;
    } else {
      const frameSessionId = sessionFrameId(frame);
      if (
        frameSessionId !== null &&
        (activeSession.current === null || activeSession.current === frameSessionId)
      ) {
        // The native reader can only receive a named session frame after the
        // daemon has attached that connection to the session. Keep imperative
        // command guards aligned with the reducer's identical proof.
        if (activeSession.current === null) {
          activeSession.current = frameSessionId;
        }
        confirmedAttachment.current = frameSessionId;
      }
    }
    dispatch({ type: "frame", frame });
    // Workflow and blackboard frames are not session-scoped, so the session
    // reducer has nowhere to put them; the panels showing those runs and boards
    // subscribe to the bus instead.
    publishFrame(frame);
    // Same authoritative frames the store folds; read here for the two kinds
    // that block a human.
    notifier.current?.observeFrame(frame);
  }, []);

  const commitAttachment = useCallback((
    pending: PendingAttachment,
    preserveProjection: boolean,
  ): boolean => {
    if (pendingAttachment.current !== pending) {
      return false;
    }
    pendingAttachment.current = null;
    freshSession.current = false;
    confirmedAttachment.current = pending.sessionId;
    activeSession.current = pending.sessionId;
    dispatch({
      type: preserveProjection ? "session-reattached" : "session-selected",
      sessionId: pending.sessionId,
    });
    for (const frame of pending.frames) {
      deliverFrame(frame);
    }
    return true;
  }, [deliverFrame]);

  /**
   * Attach on one exact native connection and commit only after acceptance.
   *
   * The native client can accept a compact Snapshot, emit it, and then fail
   * while paging the older transcript. That is an incomplete HISTORY read, not
   * a refused attach: the daemon is already on the new session. A buffered
   * Snapshot is therefore the acceptance receipt and commits the selection;
   * its missing range is handed to the ordinary retryable gap repair path.
   */
  const performAttachment = useCallback(async (
    sessionId: string,
    requiredClient?: DesktopTransport,
    preserveProjection = false,
  ): Promise<boolean> => {
    const client = requiredClient ?? transport.current;
    if (!client || (requiredClient !== undefined && transport.current !== requiredClient)) {
      if (!requiredClient) {
        dispatch({ type: "command-failed", message: NO_SHELL_DETAIL });
      }
      return false;
    }

    const pending: PendingAttachment = {
      generation: attachmentGeneration.current + 1,
      sessionId,
      frames: [],
    };
    attachmentGeneration.current = pending.generation;
    pendingAttachment.current = pending;
    dispatch({ type: "session-attach-started", sessionId });

    try {
      await client.attachSession(sessionId);
      if (pendingAttachment.current !== pending) {
        return false;
      }
      if (transport.current !== client) {
        pendingAttachment.current = null;
        dispatch({ type: "session-attach-failed", sessionId });
        dispatch({
          type: "command-failed",
          message: `Session ${sessionId} finished attaching on a connection that has already been replaced.`,
        });
        return false;
      }
      return commitAttachment(pending, preserveProjection);
    } catch (error) {
      if (pendingAttachment.current !== pending) {
        return false;
      }
      if (transport.current !== client) {
        pendingAttachment.current = null;
        dispatch({ type: "session-attach-failed", sessionId });
        return false;
      }

      const acceptedThrough = acceptedAttachmentThrough(pending);
      if (acceptedThrough !== null) {
        const committed = commitAttachment(pending, preserveProjection);
        if (committed) {
          dispatch({
            type: "session-history-incomplete",
            sessionId,
            through: acceptedThrough,
          });
          dispatch({
            type: "command-failed",
            message: `Session ${sessionId} attached, but its complete history could not be restored yet: ${describe(error)}`,
          });
        }
        return committed;
      }

      pendingAttachment.current = null;
      dispatch({ type: "session-attach-failed", sessionId });
      dispatch({ type: "command-failed", message: describe(error) });
      return false;
    }
  }, [commitAttachment]);

  const enqueueAttachment = useCallback((
    sessionId: string,
    requiredClient?: DesktopTransport,
    preserveProjection = false,
  ): Promise<boolean> => {
    const run = () => performAttachment(sessionId, requiredClient, preserveProjection);
    const previous = attachmentQueue.current;
    // Start the first item synchronously so the pending ref and render-visible
    // gate exist before any caller can issue a session-scoped command.
    const queued = previous ? previous.then(run, run) : run();
    const tail = queued.then(() => undefined, () => undefined);
    attachmentQueue.current = tail;
    void tail.finally(() => {
      if (attachmentQueue.current === tail) {
        attachmentQueue.current = null;
      }
    });
    return queued;
  }, [performAttachment]);

  const loadInbox = useCallback(async (query?: InboxListQuery) => {
    const client = transport.current;
    if (!client || !connectionReady.current) {
      dispatch({ type: "inbox-unavailable", detail: "Reconnect to read the inbox." });
      return;
    }
    const connection = connectionGeneration.current;
    const request = ++inboxReadGeneration.current;
    const current = () => connection === connectionGeneration.current && request === inboxReadGeneration.current;
    try {
      const page = await client.listInbox(query);
      if (current()) dispatch({ type: "inbox-loaded", entries: page.items });
    } catch (error) {
      if (current()) dispatch({ type: "inbox-unavailable", detail: describe(error) });
    }
  }, []);

  useEffect(() => {
    let live = true;
    connectionGeneration.current += 1;
    connectionReady.current = false;
    const client = factory.current();
    transport.current = client;
    setBridge(client);
    confirmedAttachment.current = null;
    // Work queued for the retired native connection cannot establish anything
    // about this one. Supersede it immediately rather than making reconnect
    // wait for a dead command's timeout.
    pendingAttachment.current = null;
    attachmentQueue.current = null;
    launchFrames.current = null;

    if (!client) {
      dispatch({ type: "shell-missing", detail: NO_SHELL_DETAIL });
      return;
    }

    dispatch({ type: "connecting", detail: "Connecting to codypendentd…" });
    // This attempt's identity, so its teardown cannot close a later one.
    const generation = connectionGeneration.current;
    const attempt = client.connect((frame) => {
      if (live) {
        const pending = pendingAttachment.current;
        const frameSessionId = sessionFrameId(frame);
        if (pending && frameSessionId === pending.sessionId) {
          pending.frames.push(frame);
          return;
        }
        if (pending && frame.kind === "event" && frame.session_id === null) {
          // During a handoff a session-less event cannot be attributed to the
          // old or new attachment safely. Dropping it is preferable to folding
          // it into the wrong transcript; the accepted attach's durable replay
          // restores any session event that mattered.
          return;
        }
        if (launchFrames.current && frameSessionId !== null && frameSessionId !== activeSession.current) {
          launchFrames.current.push(frame);
          return;
        }
        deliverFrame(frame);
      }
    }, generation);
    attempt
      .then(async (info) => {
        if (!live) {
          return;
        }
        connectionReady.current = true;
        reconnectAttempts.current = 0;
        dispatch({ type: "connected", info });
        settleReconnect();
        // A reconnect starts UNATTACHED: re-attach the session the operator
        // was on, or its transcript stays blank until they pick it again.
        // `null` on the first connect — there is nothing to resume.
        const resume = activeSession.current;
        if (resume) {
          await enqueueAttachment(resume, client, true);
        }
        try {
          const sessions = await client.listSessions();
          if (live) {
            rememberTitles(sessionTitles.current, sessions);
            dispatch({ type: "sessions", sessions });
          }
        } catch (error) {
          // Listing is not the connection: a daemon that answered the
          // handshake but refused this command is still connected.
          if (live) {
            dispatch({ type: "command-failed", message: describe(error) });
          }
        }
        if (live) await loadInbox();
      })
      .catch(async (error) => {
        if (!live) {
          return;
        }
        let socket = "";
        try {
          socket = await client.socketPath();
        } catch {
          socket = "";
        }
        if (!live) return;
        const detail = socket
          ? `No daemon on ${socket}: ${describe(error)}`
          : `No daemon: ${describe(error)}`;
        dispatch({ type: "connect-failed", detail });
        settleReconnect(new Error(detail));
      });

    return () => {
      live = false;
      connectionReady.current = false;
      connectionGeneration.current += 1;
      // Disconnect only once the connect attempt has SETTLED: tearing down
      // mid-handshake races the shell's connection setup, and StrictMode's
      // dev double-mount hits exactly that sequence.
      // Close THIS attempt, not whatever is open by the time this lands. The
      // teardown is deferred until the connect settles, so without the
      // generation it could shut down the connection that replaced it — and a
      // deliberate disconnect emits no `Disconnected` frame, so the store would
      // sit at "connected" while every command timed out and the reconnect
      // effect, which only fires on "disconnected", never ran again.
      void attempt
        .catch(() => undefined)
        .then(() => client.disconnect(generation).catch(() => undefined));
    };
  }, [deliverFrame, enqueueAttachment, loadInbox, reconnectTick, settleReconnect]);

  /**
   * Reconnect on demand, for a surface that needs the daemon to pick up a
   * change made outside the connection.
   *
   * The repository is fixed at connect (`DaemonClient::connect` anchors it
   * once), so rebinding to a different checkout requires a fresh connection.
   * `RepoPicker` has always had an `onReconnect` prop and a button behind it —
   * the button simply could never render, because nothing passed the prop.
   */
  const reconnect = useCallback(async () => {
    // A deliberate retry starts the backoff over: the next automatic attempt
    // after a manual one should come quickly, not after the 15 s tail.
    reconnectAttempts.current = 0;
    // Only the newest request is answered; an earlier waiter is told so
    // rather than being resolved, which would report a success it never saw.
    settleReconnect(new Error("superseded by a newer reconnect"));
    const settled = new Promise<void>((resolve, reject) => {
      pendingReconnect.current = { resolve, reject };
    });
    setReconnectTick((tick) => tick + 1);
    // Resolves when the attempt this bumped has actually CONNECTED, and
    // rejects with the daemon's own detail when it failed. Bumping the tick
    // and resolving immediately let a caller announce "reconnected" before
    // the handshake had started, and say the same thing when it failed.
    return settled;
  }, [settleReconnect]);

  const startDaemon = useCallback(async (): Promise<{ started: boolean; detail: string }> => {
    const client = transport.current;
    if (!client?.startDaemon) {
      return {
        started: false,
        detail: client
          ? "This shell cannot start the daemon itself. Run `codypendent daemon start` in a terminal."
          : NO_SHELL_DETAIL,
      };
    }
    try {
      const outcome = await client.startDaemon();
      // The socket answered: connect now rather than waiting out the backoff.
      reconnectAttempts.current = 0;
      setReconnectTick((tick) => tick + 1);
      return {
        started: true,
        detail:
          outcome.outcome === "started"
            ? `Started ${outcome.program} (pid ${outcome.pid}). Connecting…`
            : "A daemon was already running. Connecting…",
      };
    } catch (error) {
      return { started: false, detail: describe(error) };
    }
  }, []);

  // Repair a hole in the live event stream.
  //
  // A gap means events this client never received; the reducer records the
  // range rather than logging it and moving on, because a transcript that is
  // silently short by a few events is indistinguishable from one that is
  // complete. Read the range back from the durable log and fold it in — the
  // reducer de-duplicates by sequence, so re-delivering an event the stream
  // later provides anyway is harmless.
  const pendingGap = state.pendingGap;
  const connectionEpoch = state.connectionEpoch;
  useEffect(() => {
    const client = transport.current;
    if (!pendingGap || !client?.readSessionEventRange) {
      return;
    }
    let live = true;
    void (async () => {
      try {
        const events = await client.readSessionEventRange!(
          pendingGap.sessionId,
          pendingGap.after,
          pendingGap.through,
        );
        if (!live || freshSession.current || activeSession.current !== pendingGap.sessionId) {
          return;
        }
        dispatch({
          type: "frame",
          frame: {
            kind: "history",
            session_id: pendingGap.sessionId,
            through: pendingGap.through,
            events,
          },
        });
        dispatch({
          type: "gap-repaired",
          sessionId: pendingGap.sessionId,
          through: pendingGap.through,
        });
      } catch (error) {
        if (live && !freshSession.current && activeSession.current === pendingGap.sessionId) {
          // Reported, not swallowed: an unrepairable gap is a transcript the
          // operator should not read as complete. The gap stays recorded, so a
          // later reconnect re-attempts it.
          dispatch({
            type: "command-failed",
            message: `missing transcript events ${pendingGap.after + 1}–${pendingGap.through} could not be restored: ${describe(error)}`,
          });
        }
      }
    })();
    return () => {
      live = false;
    };
  }, [connectionEpoch, pendingGap]);

  const status = state.status;
  useEffect(() => {
    // A dropped socket is not terminal: retry the SAME connect flow as mount
    // (via `reconnectTick`) with backoff until the store reports connected
    // again. The shell tears the old connection down on a fresh
    // `daemon_connect`, so re-running it is safe. `shell-missing` has no
    // transport at all, so there is nothing to retry.
    if (status !== "disconnected" || transport.current === null) {
      return;
    }
    const attempt = reconnectAttempts.current;
    reconnectAttempts.current += 1;
    const delay = RETRY_DELAYS_MS[Math.min(attempt, RETRY_DELAYS_MS.length - 1)];
    const timer = window.setTimeout(() => setReconnectTick((tick) => tick + 1), delay);
    return () => window.clearTimeout(timer);
  }, [status, reconnectTick]);

  const submit = useCallback(async (objective: string): Promise<boolean> => {
    const client = transport.current;
    if (!client) {
      dispatch({ type: "command-failed", message: NO_SHELL_DETAIL });
      return false;
    }
    const blocked = pendingAttachment.current;
    if (blocked) {
      dispatch({ type: "command-failed", message: attachmentBlockedDetail(blocked.sessionId) });
      return false;
    }
    if (!freshSession.current && activeSession.current !== null && confirmedAttachment.current === null) {
      dispatch({ type: "command-failed", message: attachmentUnavailableDetail() });
      return false;
    }
    // A launch is not visible in `isRunning` until the daemon's
    // `RunStateChanged` arrives, several round trips later. Without this
    // guard a double-click on Send — or on a failure card's Retry, which is
    // enabled precisely while nothing is running — starts two sessions and
    // two paid runs, and the second silently steals the attachment.
    if (launchInFlight.current) {
      return false;
    }
    launchInFlight.current = true;
    const frames: DaemonFrame[] = [];
    launchFrames.current = frames;
    const connection = connectionGeneration.current;
    const attachment = attachmentGeneration.current;
    try {
      const handle = await client.startObjective(objective, confirmedAttachment.current);
      if (connection !== connectionGeneration.current || attachment !== attachmentGeneration.current) {
        return false;
      }
      launchFrames.current = null;
      freshSession.current = false;
      confirmedAttachment.current = handle.session_id;
      activeSession.current = handle.session_id;
      dispatch({ type: "session-reattached", sessionId: handle.session_id });
      for (const frame of frames) {
        if (sessionFrameId(frame) === handle.session_id) deliverFrame(frame);
      }
      dispatch({ type: "run-submitted", handle, objective });
      try {
        const sessions = await client.listSessions();
        rememberTitles(sessionTitles.current, sessions);
        dispatch({ type: "sessions", sessions });
      } catch {
        // Ignore session refresh failure
      }
      return true;
    } catch (error) {
      dispatch({ type: "command-failed", message: describe(error) });
      return false;
    } finally {
      if (launchFrames.current === frames) launchFrames.current = null;
      launchInFlight.current = false;
    }
  }, [deliverFrame]);

  const newSession = useCallback(() => {
    if (launchInFlight.current || state.isRunning || pendingAttachment.current) {
      dispatch({ type: "command-failed", message: "Finish or cancel the current run before starting a new chat." });
      return false;
    }
    attachmentGeneration.current += 1;
    freshSession.current = true;
    confirmedAttachment.current = null;
    activeSession.current = null;
    dispatch({ type: "new-session" });
    return true;
  }, [state.isRunning]);

  const activeRunId = state.activeRunId;
  const cancel = useCallback(async () => {
    const blocked = pendingAttachment.current;
    if (blocked) {
      dispatch({ type: "command-failed", message: attachmentBlockedDetail(blocked.sessionId) });
      return;
    }
    if (confirmedAttachment.current === null) {
      dispatch({ type: "command-failed", message: attachmentUnavailableDetail() });
      return;
    }
    const client = transport.current;
    if (!client || !activeRunId) {
      return;
    }
    try {
      await client.cancelRun(activeRunId);
    } catch (error) {
      dispatch({ type: "command-failed", message: describe(error) });
    }
  }, [activeRunId]);

  const steer = useCallback(async (text: string): Promise<SteerOutcome> => {
    const client = transport.current;
    const fail = (detail: string): SteerOutcome => {
      dispatch({ type: "command-failed", message: detail });
      return { accepted: false, detail };
    };
    if (!client) {
      return fail(NO_SHELL_DETAIL);
    }
    const blocked = pendingAttachment.current;
    if (blocked) {
      return fail(attachmentBlockedDetail(blocked.sessionId));
    }
    if (confirmedAttachment.current === null) {
      return fail(attachmentUnavailableDetail());
    }
    if (!client.queueSteering) {
      return fail("This build's bridge does not offer `queue_steering`, so steering cannot be sent.");
    }
    if (!activeRunId) {
      // Steering targets a run id. Without one there is nothing to steer, and
      // guessing a run would steer the wrong one.
      return fail("No live run to steer: the daemon has not named a run id for this session.");
    }
    if (!text.trim()) {
      return fail("Steering text cannot be empty.");
    }
    try {
      await client.queueSteering(activeRunId, text);
      return { accepted: true, at: new Date().toISOString() };
    } catch (error) {
      return fail(describe(error));
    }
  }, [activeRunId]);

  /**
   * Send one run-lifecycle command for the live run.
   *
   * Deliberately does NOT re-check whether the transition is legal: the daemon
   * owns that rule (`validate_run_transition`), and a refusal arrives here as
   * its own `run.invalid-transition` message, which is reported verbatim. What
   * this DOES check is whether the command can be sent at all — no shell, no
   * handler on this build, no run id — because those are facts about the
   * client, and reporting them as a daemon refusal would be a lie.
   */
  const sendRunLifecycle = useCallback(
    async (verb: "pause" | "resume") => {
      const client = transport.current;
      if (!client) {
        dispatch({ type: "command-failed", message: NO_SHELL_DETAIL });
        return;
      }
      const blocked = pendingAttachment.current;
      if (blocked) {
        dispatch({ type: "command-failed", message: attachmentBlockedDetail(blocked.sessionId) });
        return;
      }
      if (confirmedAttachment.current === null) {
        dispatch({ type: "command-failed", message: attachmentUnavailableDetail() });
        return;
      }
      const send = verb === "pause" ? client.pauseRun : client.resumeRun;
      if (!send) {
        dispatch({
          type: "command-failed",
          message: `This build's bridge does not offer \`${verb}_run\`, so the run cannot be ${verb}d.`,
        });
        return;
      }
      if (!activeRunId) {
        dispatch({
          type: "command-failed",
          message: `No live run to ${verb}: the daemon has not named a run id for this session.`,
        });
        return;
      }
      try {
        await send.call(client, activeRunId);
      } catch (error) {
        dispatch({ type: "command-failed", message: describe(error) });
      }
    },
    [activeRunId],
  );

  const pauseRun = useCallback(() => sendRunLifecycle("pause"), [sendRunLifecycle]);
  const resumeRun = useCallback(() => sendRunLifecycle("resume"), [sendRunLifecycle]);

  /**
   * Run one queue mutation and report whether the daemon accepted it.
   *
   * A failure lands in `promptQueueError` as well as the general `error`, so
   * the queue panel can say "this command failed" in a place an operator is
   * already looking — and so a failed mutation never renders as an empty queue.
   */
  const runQueueMutation = useCallback(
    async (what: string, call: ((client: DesktopTransport) => Promise<void>) | null) => {
      const client = transport.current;
      if (!client) {
        dispatch({ type: "prompt-queue-failed", detail: NO_SHELL_DETAIL });
        dispatch({ type: "command-failed", message: NO_SHELL_DETAIL });
        return false;
      }
      const blocked = pendingAttachment.current;
      if (blocked) {
        const detail = attachmentBlockedDetail(blocked.sessionId);
        dispatch({ type: "prompt-queue-failed", detail });
        dispatch({ type: "command-failed", message: detail });
        return false;
      }
      if (confirmedAttachment.current === null) {
        const detail = "No session is confirmed on this connection; attach or start a session before changing its prompt queue.";
        dispatch({ type: "prompt-queue-failed", detail });
        dispatch({ type: "command-failed", message: detail });
        return false;
      }
      if (!call) {
        const detail = `This build's bridge does not offer \`${what}\`, so the prompt queue cannot be changed.`;
        dispatch({ type: "prompt-queue-failed", detail });
        dispatch({ type: "command-failed", message: detail });
        return false;
      }
      const connection = connectionGeneration.current;
      const attachment = attachmentGeneration.current;
      const session = confirmedAttachment.current;
      const current = () => connection === connectionGeneration.current
        && attachment === attachmentGeneration.current && session === confirmedAttachment.current;
      try {
        await call(client);
        if (!current()) return false;
        dispatch({ type: "prompt-queue-accepted" });
        return true;
      } catch (error) {
        if (!current()) return false;
        dispatch({ type: "prompt-queue-failed", detail: describe(error) });
        dispatch({ type: "command-failed", message: describe(error) });
        return false;
      }
    },
    [],
  );

  const queuePrompt = useCallback(
    async (text: string, delivery: PromptDelivery = { type: "Queue" }) => {
      if (!text.trim()) {
        // The daemon rejects blank text `prompt-queue.empty`; there is nothing
        // to learn from the round trip.
        const detail = "A queued prompt cannot be empty.";
        dispatch({ type: "prompt-queue-failed", detail });
        return false;
      }
      const client = transport.current;
      return runQueueMutation(
        "queue_prompt",
        client?.queuePrompt ? (c) => c.queuePrompt!(text.trim(), delivery) : null,
      );
    },
    [runQueueMutation],
  );

  const updateQueuedPrompt = useCallback(
    async (promptId: string, text?: string | null, delivery?: PromptDelivery | null) => {
      if (typeof text === "string" && !text.trim()) {
        const detail = "A queued prompt cannot be emptied — remove it instead.";
        dispatch({ type: "prompt-queue-failed", detail });
        return false;
      }
      const client = transport.current;
      return runQueueMutation(
        "update_queued_prompt",
        client?.updateQueuedPrompt
          ? (c) =>
              c.updateQueuedPrompt!(
                promptId,
                typeof text === "string" ? text.trim() : text,
                delivery,
              )
          : null,
      );
    },
    [runQueueMutation],
  );

  const promoteQueuedPrompt = useCallback(
    async (promptId: string) => {
      const client = transport.current;
      return runQueueMutation(
        "promote_queued_prompt",
        client?.promoteQueuedPrompt ? (c) => c.promoteQueuedPrompt!(promptId) : null,
      );
    },
    [runQueueMutation],
  );

  const deleteQueuedPrompt = useCallback(
    async (promptId: string) => {
      const client = transport.current;
      return runQueueMutation(
        "delete_queued_prompt",
        client?.deleteQueuedPrompt ? (c) => c.deleteQueuedPrompt!(promptId) : null,
      );
    },
    [runQueueMutation],
  );

  const selectSession = useCallback(async (sessionId: string) => {
    return enqueueAttachment(sessionId);
  }, [enqueueAttachment]);

  // The decision card owns pending/refusal feedback. Preserve the promise so
  // it cannot mistake a swallowed rejection for an accepted decision.
  const resolveApproval = useCallback(async (approvalId: string, decision: ApprovalChoice, scope: ApprovalGrant = "Once") => {
    const client = transport.current;
    if (!client || !connectionReady.current) throw new Error("Reconnect before deciding this approval.");
    await client.resolveApproval(approvalId, decision, scope);
  }, []);

  const resolveQuestion = useCallback(async (questionId: string, outcome: QuestionOutcomeView) => {
    const client = transport.current;
    if (!client || !connectionReady.current) throw new Error("Reconnect before answering this question.");
    if (!client.resolveQuestion) throw new Error("This shell cannot answer questions; answer from the TUI.");
    await client.resolveQuestion(questionId, outcome);
  }, []);

  const mutateInbox = useCallback(async (entryId: string, type: "Acknowledge" | "Dismiss") => {
    const client = transport.current;
    if (!client || !connectionReady.current) throw new Error("Reconnect to update the inbox.");
    const connection = connectionGeneration.current;
    // A list requested before a mutation cannot later restore the old row.
    inboxReadGeneration.current += 1;
    const entry = await client.mutateInbox({ type, entry_id: entryId });
    if (connection !== connectionGeneration.current) return;
    inboxReadGeneration.current += 1;
    dispatch({ type: "inbox-entry-updated", entry });
  }, []);
  const acknowledgeInbox = useCallback((entryId: string) => mutateInbox(entryId, "Acknowledge"), [mutateInbox]);
  const dismissInbox = useCallback((entryId: string) => mutateInbox(entryId, "Dismiss"), [mutateInbox]);

  // Analytics owns its request generations and error presentation. A retired
  // filter/connection must not leak its eventual failure into the app banner.
  const queryAnalytics = useCallback(async (query?: AnalyticsQuery) => {
    const client = transport.current;
    return client ? client.queryAnalytics(query) : null;
  }, []);

  const exportAnalytics = useCallback(async (request: AnalyticsExportRequest) => {
    const client = transport.current;
    return client ? client.exportAnalytics(request) : null;
  }, []);

  const readArtifact = useCallback(async (artifact: ArtifactRef) => {
    const client = transport.current;
    if (!client || !client.readArtifact) {
      dispatch({ type: "command-failed", message: NO_SHELL_DETAIL });
      return null;
    }
    try {
      return await client.readArtifact(artifact);
    } catch (error) {
      dispatch({ type: "command-failed", message: describe(error) });
      return null;
    }
  }, []);

  /** Clear the client-error banner (the operator read it). */
  const dismissError = useCallback(() => {
    dispatch({ type: "command-failed", message: null });
  }, []);

  return {
    state,
    reconnect,
    startDaemon,
    dismissError,
    submit,
    newSession,
    cancel,
    steer,
    pauseRun,
    resumeRun,
    queuePrompt,
    updateQueuedPrompt,
    promoteQueuedPrompt,
    deleteQueuedPrompt,
    selectSession,
    resolveApproval,
    resolveQuestion,
    loadInbox,
    acknowledgeInbox,
    dismissInbox,
    queryAnalytics,
    exportAnalytics,
    readArtifact,
    transport: bridge,
  };
}

/** Titles come from the daemon's own session list; nothing is invented. */
function rememberTitles(titles: Map<string, string>, sessions: readonly SessionRow[]): void {
  for (const session of sessions) {
    titles.set(session.session_id, session.title);
  }
}

/**
 * A session-scoped catch-up/history frame can only be emitted after the daemon
 * accepted the attach. Its watermark also names the durable range to retry if
 * the native command subsequently failed while paging older history.
 */
function acceptedAttachmentThrough(pending: PendingAttachment): number | null {
  let through: number | null = null;
  for (const frame of pending.frames) {
    let candidate: number | null = null;
    if (frame.kind === "catchup" && frame.snapshot.type !== "Unknown") {
      candidate = frame.snapshot.through;
    } else if (frame.kind === "history") {
      candidate = frame.through;
    }
    if (candidate !== null) {
      through = through === null ? candidate : Math.max(through, candidate);
    }
  }
  return through;
}

/** The session identity carried by a transcript-projection frame, if any. */
function sessionFrameId(frame: DaemonFrame): string | null {
  switch (frame.kind) {
    case "event":
      return frame.session_id;
    case "catchup":
    case "history":
      return frame.session_id;
    case "remote_ui":
    case "workflow_event":
    case "blackboard_posted":
    case "disconnected":
      return null;
  }
}

function attachmentBlockedDetail(sessionId: string): string {
  return `Session ${sessionId} is still attaching; wait for it to finish before sending a session-scoped command.`;
}

function attachmentUnavailableDetail(): string {
  return "No session is confirmed on this connection; reopen the session before sending a session-scoped command.";
}

function describe(error: unknown): string {
  if (typeof error === "string") {
    return error;
  }
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}
