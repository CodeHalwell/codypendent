import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { QuestionOutcomeView, SessionId } from "./types.js";
import { Navigation, NAV_GROUPS, type DesktopView } from "./components/Navigation.js";
import { NativeAgent } from "./agui/NativeAgent.js";
import { useDesktopTools } from "./agui/useDesktopTools.js";
import { AgentToolsView } from "./agui/AgentToolsView.js";
import type { DesktopContext } from "./agui/types.js";
import type { ArtifactRef } from "@codypendent/protocol";
import type { ApprovalGrant } from "./approval.js";
import { Transcript } from "./components/Transcript.js";
import { Composer } from "./components/Composer.js";
import { PlanHandoff } from "./components/PlanHandoff.js";
import { completedPlanHandoff } from "./planHandoff.js";
import { createWorkflowLaunchMemory } from "./components/workflowInputs.js";
import { Steering } from "./components/Steering.js";
import { PromptQueue } from "./components/PromptQueue.js";
import { usePromptQueueActions } from "./usePromptQueueActions.js";
import { ConfirmCancel, runAtStake } from "./components/ConfirmCancel.js";
import { ConnectionBanner } from "./components/ConnectionBanner.js";
import { ViewBar } from "./components/ViewBar.js";
import { WorkspaceBar } from "./components/WorkspaceBar.js";
import { preferredWorkspaceEditor } from "./components/WorkspaceLauncher.js";
import { useNativeDesktopActions } from "./useNativeDesktopActions.js";
import { useWorkspacePreferences } from "./useWorkspacePreferences.js";
import { InboxView } from "./components/InboxView.js";
import { CommandPalette, type PaletteEntry } from "./components/CommandPalette.js";
import { ShortcutsCard } from "./components/ShortcutsCard.js";
import { SkillsView } from "./components/SkillsView.js";
import { MemoryView } from "./components/MemoryView.js";
import { PluginsView } from "./components/PluginsView.js";
import { ContextView } from "./components/ContextView.js";
import { BacktrackView } from "./components/BacktrackView.js";
import { SessionLibrary } from "./components/SessionLibrary.js";
import { RepoPicker } from "./components/RepoPicker.js";
import { CouncilBrowser } from "./components/CouncilBrowser.js";
import { ModelPicker } from "./components/ModelPicker.js";
import { ProviderPicker } from "./components/ProviderPicker.js";
import type { ProviderRow, RunDefaults } from "./components/localConfig.js";
import { ApiKeys } from "./components/ApiKeys.js";
import { ModePicker } from "./components/ModePicker.js";
import {
  Onboarding,
  onboardingSkipped,
  readOnboardingStatus,
  setOnboardingSkipped,
  shouldOpenOnboarding,
} from "./components/Onboarding.js";
import { localConfigClient, shellAvailable } from "./components/localConfig";
import type { RepositorySelection } from "./localConfig.js";
import {
  missingBridge,
  unloaded,
  type DocCard,
  type KnowledgeTransport,
  type LearningCard,
  type LearningMutation,
  type Loaded,
  type MemoryCard,
  type SkillCard,
  type UiPluginLifecycleStatus,
} from "./components/knowledgeTransport.js";
import { useDaemon } from "./useDaemon.js";
import { useConnectionPresentation } from "./useConnectionPresentation.js";
import { useModelRecovery } from "./useModelRecovery.js";
import { newCouncilDraft, useCouncilRun } from "./useCouncilRun.js";
import { runLifecycleAffordance } from "./daemonState.js";
import { createKnowledgeTransport, type DesktopTransport } from "./transport.js";
import type { NotificationSink } from "./osNotifications.js";
import { useCommandActions } from "./useCommandActions.js";
import type { InboxDeepLink, InboxEntry, PublishTarget } from "@codypendent/protocol";

/** Load secondary tools only when their workspace is opened. */
const AnalyticsDashboard = React.lazy(() => import("./components/AnalyticsDashboard.js").then((module) => ({ default: module.AnalyticsDashboard })));
const DocsView = React.lazy(() => import("./components/DocsView.js").then((module) => ({ default: module.DocsView })));
const EdgesView = React.lazy(() => import("./components/EdgesView.js").then((module) => ({ default: module.EdgesView })));
const WorkflowView = React.lazy(() => import("./components/WorkflowView.js").then((module) => ({ default: module.WorkflowView })));
const KanbanView = React.lazy(() => import("./components/KanbanView.js").then((module) => ({ default: module.KanbanView })));
const BlackboardView = React.lazy(() => import("./components/BlackboardView.js").then((module) => ({ default: module.BlackboardView })));
const CouncilBuilder = React.lazy(() => import("./components/CouncilBuilder.js").then((module) => ({ default: module.CouncilBuilder })));
const CouncilResults = React.lazy(() => import("./components/CouncilResults.js").then((module) => ({ default: module.CouncilResults })));

/** The bridge commands each surface needs, named in its unavailable panel. */
const REQUIRED_COMMANDS = {
  skills: ["list_skills"],
  memories: ["list_memories", "correct_memory", "forget_memory"],
  learnings: ["list_learnings", "mutate_learning"],
  docs: [
    "list_documents",
    "create_document",
    "acquire_document_lease",
    "mutate_document",
    "release_document_lease",
    "publish_document",
  ],
  plugins: [
    "list_ui_plugins",
    "smoke_test_ui_plugin",
    "enable_ui_plugin",
    "approve_ui_plugin_update",
    "reject_ui_plugin_update",
    "revoke_ui_plugin",
  ],
} as const;

/** Load extension rendering only after a confirmed session attachment. */
const RemoteUiHost = React.lazy(() => import("./components/RemoteUiHost.js")
  .then((module) => ({ default: module.RemoteUiHost })));

/**
 * The views that read through the knowledge transport. Without one they are
 * honest dead ends, and the sidebar and palette say so up front.
 */
const KNOWLEDGE_VIEWS: ReadonlySet<DesktopView> = new Set<DesktopView>([
  "skills",
  "memory",
  "docs",
  "plugins",
]);

interface AppProps {
  /**
   * How to reach `codypendentd`. Defaults to the Tauri shell bridge, which
   * yields `null` outside the shell — the app then shows a disconnected state
   * with the reason. Tests inject a stub to drive a connected client.
   */
  makeTransport?: () => DesktopTransport | null;
  initialView?: DesktopView;
  /**
   * Where OS notifications for blocking work go. Omitted in the app, which
   * sends them through Tauri's notification plugin; a test injects a sink to
   * assert what the user would actually have been shown.
   */
  notify?: NotificationSink;
  /**
   * The knowledge surfaces' call surface (Skills, Memory, Docs, UI plugins).
   *
   * Defaults to the shell's transport (`createKnowledgeTransport`), which is
   * `null` outside the shell — a browser tab, a test. Each surface then
   * renders an explicit unavailable panel naming the commands it is waiting
   * for, never an empty list, which would assert there is nothing to show.
   * Tests inject a stub to drive the views with data.
   */
  knowledge?: KnowledgeTransport;
  /** How to reach the shell's knowledge commands; overridden only in tests. */
  makeKnowledge?: () => KnowledgeTransport | null;
}

/** Native menu shortcuts must not move focus behind a confirmation dialog. */
function blockingModalOpen(): boolean {
  return Array.from(document.querySelectorAll('[aria-modal="true"]')).some((modal) => {
    const label = modal.getAttribute("aria-label");
    return label !== "Command palette" && label !== "Keyboard shortcuts" && label !== "Workspace sidebar";
  });
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

/** Read a surface, or record exactly why it could not be read. */
export async function read<T>(
  fetcher: (() => Promise<T[]>) | undefined,
  commands: readonly string[],
  set: React.Dispatch<React.SetStateAction<Loaded<T>>>,
  /**
   * Whether this read's answer is still wanted by the time it arrives.
   *
   * Dropping a repository-scoped surface on a reconnect is not enough on its
   * own: a query started under the old connection is still in flight, and if
   * it settles AFTER the new one it writes the previous checkout's records
   * back over them. The screen then shows one repository's documents while
   * every mutation addresses another, and a block delete addresses a document
   * by id alone.
   */
  stillWanted: () => boolean = () => true,
): Promise<void> {
  if (!fetcher) {
    set({ items: [], status: "unavailable", detail: missingBridge(commands) });
    return;
  }
  set({ items: [], status: "loading", detail: null });
  try {
    const items = await fetcher();
    if (!stillWanted()) {
      return;
    }
    set({ items, status: "loaded", detail: null });
  } catch (error) {
    if (!stillWanted()) {
      return;
    }
    // A failed read is not an empty read. The surface says so.
    set({ items: [], status: "unavailable", detail: describe(error) });
  }
}

export const App: React.FC<AppProps> = ({
  makeTransport,
  initialView = "sessions",
  notify,
  knowledge: injectedKnowledge,
  makeKnowledge = createKnowledgeTransport,
}) => {
  // Resolved once: the shell is either there or not for the life of the app,
  // and a transport re-created per render would re-read every surface.
  const [knowledge] = useState<KnowledgeTransport | undefined>(
    () => injectedKnowledge ?? makeKnowledge() ?? undefined,
  );
  const [currentView, setCurrentView] = useState<DesktopView>(initialView);
  const viewIdentity = useRef({ view: currentView });
  if (viewIdentity.current.view !== currentView) viewIdentity.current = { view: currentView };
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [shortcutsOpen, setShortcutsOpen] = useState(false);
  const sidebarToggleRef = useRef<HTMLButtonElement>(null);
  const [composerFocusRequested, setComposerFocusRequested] = useState(false);
  const { sidebarVisible, toggleSidebar, closeSidebar, compact, appearance, setAppearance } = useWorkspacePreferences();
  /**
   * Whether the operator has picked a view themselves yet.
   *
   * The first-run gate below runs once, asynchronously, and must never yank
   * somebody off a view they deliberately opened while it was reading.
   */
  const chosenView = useRef(false);
  /**
   * Where Escape goes back to.
   *
   * The TUI has one working surface and summons everything else over it, so
   * there is always a way out. The desktop had 22 sidebar destinations and no
   * way back at all — every secondary view was a dead end you had to leave by
   * aiming at the sidebar again. This is that way out.
   */
  const viewHistory = useRef<DesktopView[]>([]);
  /** A provider chosen on the Providers page, handed to the add-model flow. */
  const [pendingProvider, setPendingProvider] = useState<ProviderRow | null>(null);
  const selectView = useCallback(
    (view: DesktopView) => {
      if (compact) closeSidebar();
      chosenView.current = true;
      setFocusDecision(null);
      // The history push happens here, not inside the `setCurrentView`
      // updater: StrictMode double-invokes updaters, which would push twice.
      if (currentView !== view) {
        viewHistory.current.push(currentView);
        // A wandering session should not grow an unbounded stack.
        if (viewHistory.current.length > 32) {
          viewHistory.current.shift();
        }
      }
      // A provider chosen on the Providers page opens the add-model form on
      // the NEXT visit to Models. Leaving for anywhere else abandons that
      // choice, or the form re-opened on a provider the operator had moved on
      // from every time they came back.
      if (view !== "models") {
        setPendingProvider(null);
      }
      setCurrentView(view);
    },
    [currentView, compact, closeSidebar],
  );
  /** Escape: pop back, or fall back to the session — never a dead end. */
  const goBack = useCallback(() => {
    chosenView.current = true;
    setFocusDecision(null);
    // The history pop likewise stays out of the updater (double-invoke would
    // pop twice).
    const target = viewHistory.current.pop();
    if (target !== undefined && target !== currentView) {
      setCurrentView(target);
      return;
    }
    setCurrentView(currentView === "sessions" ? currentView : "sessions");
  }, [currentView]);
  /** The "stop opening setup automatically" preference (see `Onboarding.tsx`). */
  const [skipOnboarding, setSkipOnboarding] = useState(onboardingSkipped);
  const {
    state,
    reconnect,
    startDaemon,
    dismissError,
    submit,
    newSession,
    cancel: cancelNative,
    steer,
    pauseRun: pauseNativeRun,
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
    transport,
  } = useDaemon(makeTransport, notify);
  const [workspaceStarting, setWorkspaceStarting] = useState(false);
  const connectionPresentation = useConnectionPresentation(state.status, workspaceStarting);
  const startWorkspace = useCallback(async () => {
    setWorkspaceStarting(true);
    try { return await startDaemon(); }
    finally { setWorkspaceStarting(false); }
  }, [startDaemon]);
  const commandActions = useCommandActions(`${state.connectionEpoch}:${state.status}`);
  const [focusDecision, setFocusDecision] = useState<string | null>(null);
  const [followLatestRequest, setFollowLatestRequest] = useState(0);
  const readArtifact = useCallback((artifact: ArtifactRef) => {
    if (!transport?.readArtifact) return Promise.reject(new Error("Artifact reading is unavailable on this connection."));
    return transport.readArtifact(artifact);
  }, [transport]);
  const saveArtifact = useCallback((artifact: ArtifactRef) => {
    if (!transport?.saveArtifact) return Promise.reject(new Error("Saving requires the desktop shell."));
    return transport.saveArtifact(artifact);
  }, [transport]);
  /**
   * A referentially STABLE launch-status probe.
   *
   * `ConnectionBanner` keys an effect on this, so binding it inline during
   * render handed the banner a new identity on EVERY parent render —
   * navigation, a palette toggle, a reconnect tick — and each one re-ran the
   * effect and fired another probe while disconnected. Against a silent socket
   * each call stays pending for the whole ping budget, so the probes overlap
   * and pile up. Bound once per transport instead.
   */
  const launchStatus = useMemo(
    () => transport?.daemonLaunchStatus?.bind(transport),
    [transport],
  );
  /**
   * The run whose blackboard the Blackboard panel opens on. Set by the Workflow
   * panel so "show this run's board" is one click rather than a copied id; the
   * panel still reads the board itself, so nothing is displayed that a real
   * `ReadBlackboard` did not return.
   */
  const [blackboardRunId, setBlackboardRunId] = useState<string | undefined>(undefined);
  /**
   * Whether the steering panel is open under the composer, and whether a
   * cancellation is awaiting confirmation. Both belong to the run the operator
   * is already looking at, so both live here rather than in a nav entry.
   */
  const [steeringOpen, setSteeringOpen] = useState(false);
  const [cancelPending, setCancelPending] = useState(false);
  /**
   * Whether the pending-prompt queue panel is open.
   *
   * The panel also renders unasked when there is something to say — a queue
   * with entries in it, or a mutation the daemon refused — so a failure is
   * never hidden behind a toggle the operator has not clicked.
   */
  const [queueOpen, setQueueOpen] = useState(false);
  /**
   * Set by the panel's Close button. With prompts queued, `queueVisible` used
   * to force the panel open regardless, which made Close a button that
   * flipped a flag and changed nothing. Reset whenever the queue GROWS, so a
   * new prompt still surfaces the panel.
   */
  const [queueDismissed, setQueueDismissed] = useState(false);
  const previousQueueLength = useRef(0);
  useEffect(() => {
    if (state.pendingPrompts.length > previousQueueLength.current) {
      setQueueDismissed(false);
    }
    previousQueueLength.current = state.pendingPrompts.length;
  }, [state.pendingPrompts.length]);

  /**
   * The selected repository, and the configured council names.
   *
   * Both are LOCAL configuration read through the shell, not daemon state, so
   * they are held here rather than in the session store. `repository` starts as
   * `undefined` meaning "not asked yet" — distinct from `null`, which is the
   * real answer "no repository is selected"; the Council run panel says which.
   */
  const [repository, setRepository] = useState<RepositorySelection | null | undefined>(undefined);
  const [councilNames, setCouncilNames] = useState<string[] | undefined>(undefined);
  const [councilDraft, setCouncilDraft] = useState(newCouncilDraft);
  const councilRun = useCouncilRun(transport?.runCouncil && ((name, objective, onProgress) =>
    transport.runCouncil!(name, objective, { repository: repository?.path ?? null, sessionId: state.activeSessionId }, onProgress)));
  /** True while the council builder is open in place of the browser. */
  const [buildingCouncil, setBuildingCouncil] = useState(false);
  /**
   * Configured model ids for the council builder's suggestion datalists. Read
   * lazily when the builder opens — the builder declared the prop from day
   * one and was never passed it, so the operator typed model ids from memory
   * against a store that hard-refuses unknown ones.
   */
  const [configuredModelIds, setConfiguredModelIds] = useState<string[] | undefined>(undefined);
  /**
   * The staged run defaults, for the composer's status strip. The strip used
   * to hardcode "Build Mode" whatever the Mode/Model pickers had staged;
   * re-read whenever the operator lands back on the session view, which is
   * where the label shows (and the pickers are other views, so a change
   * there is always followed by a return here).
   */
  const [runDefaultsLabel, setRunDefaultsLabel] = useState<string | null>(null);
  const [modelDiscoveryReady, setModelDiscoveryReady] = useState(!shellAvailable());
  const discoveryPromise = useRef<Promise<unknown> | null>(null);
  const selectedModel = useRef<string | null>(null);
  const defaultsReadGeneration = useRef(0);
  /** The composer draft, lifted so a view round-trip cannot lose it. */
  const [composerDraft, setComposerDraft] = useState("");
  const draftRevision = useRef(0);
  const modelRevision = useRef(0);
  const liveDraft = useRef(composerDraft);
  const [objectivePending, setObjectivePending] = useState(false);
  const pendingObjectives = useRef(0);
  const preparingPlan = useRef(false);
  const [planPreparing, setPlanPreparing] = useState(false);
  const [preparedPlan, setPreparedPlan] = useState<string | null>(null);
  const [workflowLaunchMemory] = useState(createWorkflowLaunchMemory);
  const [planPreparationError, setPlanPreparationError] = useState<string | null>(null);
  liveDraft.current = composerDraft;
  const changeComposerDraft = useCallback((text: string) => { draftRevision.current++; liveDraft.current = text; setComposerDraft(text); }, []);
  const acceptComposerDraft = useCallback((submittedDraft: string) => {
    if (liveDraft.current === submittedDraft) { liveDraft.current = ""; setComposerDraft(""); }
  }, []);
  const refreshRunDefaults = useCallback(async (stillWanted: () => boolean, onRead?: (defaults: RunDefaults) => void) => {
    const generation = ++defaultsReadGeneration.current;
    const defaults = await localConfigClient.runDefaults();
    if (generation !== defaultsReadGeneration.current || !stillWanted()) return;
    const mode = defaults.mode.type === "Unknown" ? "Build" : defaults.mode.type;
    selectedModel.current = defaults.model;
    setRunDefaultsLabel(`${mode} mode · ${defaults.model ?? "model: daemon chooses"}`);
    onRead?.(defaults);
  }, []);
  useEffect(() => {
    if (currentView !== "sessions" || !shellAvailable()) {
      return;
    }
    let cancelled = false;
    void refreshRunDefaults(() => !cancelled)
      .catch(() => {
        // The fallback constant stays; a failed read must not invent a label.
      });
    return () => {
      cancelled = true;
    };
  }, [currentView, modelDiscoveryReady, refreshRunDefaults]);
  useEffect(() => {
    if (currentView !== "council" || !buildingCouncil || !shellAvailable()) {
      return;
    }
    let cancelled = false;
    void localConfigClient
      .listModels()
      .then((view) => {
        if (!cancelled) {
          setConfiguredModelIds(view.models.map((model) => model.id));
        }
      })
      .catch(() => {
        // Unknown stays unknown: the field remains free text and the crate's
        // refusal stays the authority.
      });
    return () => {
      cancelled = true;
    };
  }, [currentView, buildingCouncil, modelDiscoveryReady]);

  const [skills, setSkills] = useState<Loaded<SkillCard>>(unloaded);
  const [memories, setMemories] = useState<Loaded<MemoryCard>>(unloaded);
  const [learnings, setLearnings] = useState<Loaded<LearningCard>>(unloaded);
  const [docs, setDocs] = useState<Loaded<DocCard>>(unloaded);
  const [plugins, setPlugins] = useState<Loaded<UiPluginLifecycleStatus>>(unloaded);
  const [memoryNotice, setMemoryNotice] = useState<string | null>(null);
  const [docsNotice, setDocsNotice] = useState<string | null>(null);
  const [pluginNotice, setPluginNotice] = useState<string | null>(null);

  const connected = state.status === "connected";
  useEffect(() => {
    if (!connected || !shellAvailable()) return;
    let live = true;
    setModelDiscoveryReady(false);
    discoveryPromise.current = localConfigClient.discoverModels?.(true).catch(() => undefined).finally(() => { if (live) setModelDiscoveryReady(true); }) ?? null;
    return () => { live = false; };
  }, [connected, state.connectionEpoch]);
  const openSourceFile = useCallback(async (file: string, line?: number, runId?: string | null) => {
    if (!transport?.openWorkspace || !transport.workspaceTargets || !repository?.path) {
      throw new Error("Choose a repository in the desktop app before opening a source file.");
    }
    const editors = (await transport.workspaceTargets()).filter((target) => target.kind === "editor");
    const preferred = preferredWorkspaceEditor();
    const editor = editors.find((target) => target.id === preferred) ?? editors[0];
    if (!editor) throw new Error("Install a supported editor to open source files.");
    await transport.openWorkspace({
      target: editor.id, expectedRepository: repository.path,
      expectedSession: runId === null ? null : state.activeSessionId, expectedRun: runId === undefined ? state.activeRunId : runId, file, line,
    });
  }, [transport, repository?.path, state.activeSessionId, state.activeRunId]);
  /**
   * Whether a run-scoped command (`CancelRun`, `QueueSteering`) can actually
   * be sent: connected, and holding the run id the daemon named. Without both
   * the buttons are not offered — an affordance that cannot reach a run is
   * worse than no affordance.
   */
  const canControlRun =
    connected &&
    state.attachingSessionId === null &&
    state.sessionAttachmentConfirmed &&
    state.activeRunId !== null;
  /** Read cancellation details only while the confirmation is open. */
  const atStake = useMemo(
    () => cancelPending ? runAtStake(state.durableEvents, state.activeRunId) : null,
    [cancelPending, state.durableEvents, state.activeRunId],
  );
  /**
   * Which of pause/resume the daemon would accept for this run right now, or
   * `null` for neither. Read from the run state the store folded off
   * `RunStateChanged`, never from `isRunning` — see `runLifecycleAffordance`.
   */
  const lifecycle = runLifecycleAffordance(state);
  /**
   * Whether a real `QueuePrompt` can be sent: connected, with a session the
   * shell is attached to, and a bridge that offers the command. A run id is
   * NOT required — the queue is session-scoped, and queueing work for a session
   * whose run has just ended is exactly the point.
   */
  const canQueuePrompts =
    connected &&
    state.attachingSessionId === null &&
    state.sessionAttachmentConfirmed &&
    state.activeSessionId !== null &&
    Boolean(transport?.queuePrompt);
  const promptQueueActions = usePromptQueueActions({
    scope: JSON.stringify([state.connectionEpoch, state.status, state.activeSessionId, state.attachingSessionId]),
    sessionId: state.activeSessionId,
    prompts: state.pendingPrompts,
    canMutate: canQueuePrompts,
    onEdit: updateQueuedPrompt,
    onPromote: promoteQueuedPrompt,
    onDelete: deleteQueuedPrompt,
  });
  /** Shown when the queue has something to say even if nobody opened it. */
  const queueVisible =
    !queueDismissed &&
    (queueOpen || state.pendingPrompts.length > 0 || state.promptQueueError !== null);

  // A run that ended or whose session is not confirmed on this connection is
  // no longer controllable, so neither affordance may outlive the boundary
  // holding a stale run id.
  useEffect(() => {
    if (!canControlRun) {
      setCancelPending(false);
      setSteeringOpen(false);
    }
  }, [canControlRun]);

  // Stable so the memoized `Navigation` — 6 groups and 22 destinations — is
  // skipped entirely while a reply streams, instead of reconciling per token.
  const openPalette = useCallback(() => {
    if (compact) closeSidebar();
    // The palette always opens on top: closing shortcuts here keeps
    // `shortcutsOpen` from going stale-true underneath it, which was letting
    // one Escape dismiss both overlays instead of just the topmost one.
    setShortcutsOpen(false);
    setPaletteOpen(true);
  }, [compact, closeSidebar]);
  const selectSessionFromNav = useCallback(
    (id: SessionId) => {
      modelRecovery.cancel();
      setFocusDecision(null);
      setCurrentView("sessions");
      void selectSession(id);
    },
    [selectSession],
  );
  const startNewChat = useCallback(() => {
    modelRecovery.cancel();
    if (!newSession()) return;
    setPaletteOpen(false);
    setShortcutsOpen(false);
    setFocusDecision(null);
    setQueueOpen(false);
    setQueueDismissed(true);
    setSteeringOpen(false);
    selectView("sessions");
    setComposerFocusRequested(true);
  }, [newSession, selectView]);

  // Referentially stable across renders. Inline arrows here would hand
  // `TranscriptRow` a new `onApprove`/`onReject` on every token and defeat its
  // memo entirely — the whole transcript would reconcile per token again.
  const approve = useCallback(
    (approvalId: string, scope: ApprovalGrant = "Once") => resolveApproval(approvalId, "approve", scope),
    [resolveApproval],
  );
  const answerQuestion = useCallback(
    (questionId: string, outcome: QuestionOutcomeView) =>
      resolveQuestion(questionId, outcome),
    [resolveQuestion],
  );
  // An accepted local message explicitly returns to the latest turn. Incoming
  // output alone still leaves a reader in the history they chose to inspect.
  const nativeSubmit = useRef(submit);
  nativeSubmit.current = submit;
  const attachedSession = useRef(state.activeSessionId);
  attachedSession.current = state.activeSessionId;
  const [agent] = useState(() => new NativeAgent({ submit: async (text) => {
    if (preparingPlan.current) return false;
    const session = attachedSession.current;
    const send = nativeSubmit.current;
    pendingObjectives.current++;
    setObjectivePending(true);
    try {
      if (shellAvailable() && !selectedModel.current) await discoveryPromise.current;
      if (attachedSession.current !== session || nativeSubmit.current !== send) return false;
      return await send(text);
    } finally {
      pendingObjectives.current--;
      if (!pendingObjectives.current) setObjectivePending(false);
    }
  }, sessionId: () => attachedSession.current }));
  useEffect(() => { agent.observeNative(); return () => agent.close(); }, [agent]);
  useEffect(() => { if (!connected) agent.close(); }, [connected, agent]);
  const previousAgentSession = useRef(state.activeSessionId);
  useEffect(() => {
    if (previousAgentSession.current !== null && previousAgentSession.current !== state.activeSessionId) {
      agent.close();
      agent.setMessages([]);
    }
    if (state.activeSessionId) agent.threadId = state.activeSessionId;
    previousAgentSession.current = state.activeSessionId;
  }, [agent, state.activeSessionId]);
  const sendWithAgent = useCallback(async (objective: string): Promise<boolean> => {
    if (preparingPlan.current) return false;
    pendingObjectives.current++;
    setObjectivePending(true);
    try {
      const accepted = await agent.submitObjective(objective);
      if (accepted) {
        setFocusDecision(null);
        setFollowLatestRequest((request) => request + 1);
      }
      return accepted;
    } finally {
      pendingObjectives.current--;
      if (!pendingObjectives.current) setObjectivePending(false);
    }
  }, [agent]);
  const modelRecovery = useModelRecovery({
    snapshot: () => ({ connected, epoch: state.connectionEpoch, sessionId: state.activeSessionId,
      attached: state.sessionAttachmentConfirmed, attaching: state.attachingSessionId !== null,
      running: state.isRunning, draftRevision: draftRevision.current, modelRevision: modelRevision.current,
      model: selectedModel.current, repository: repository?.path ?? null,
      mode: runDefaultsLabel?.split(" mode")[0] ?? "Build", view: currentView }),
    submit: sendWithAgent,
    discover: async () => shellAvailable() ? localConfigClient.discoverModels?.() : undefined,
    models: () => localConfigClient.listModels(),
    stage: async (model, current) => {
      if (!current()) return;
      await localConfigClient.setRunModel(model);
      if (!current()) return;
      selectedModel.current = model;
      setRunDefaultsLabel((label) => `${label?.split(" mode")[0] ?? "Build"} mode · ${model}`);
    },
  });
  const sendObjective = modelRecovery.submit;
  const completedPlan = useMemo(() => completedPlanHandoff(state), [state]);
  useEffect(() => { setPlanPreparationError(null); }, [completedPlan?.runId, state.activeSessionId]);
  const planBoundary = useMemo(() => ({}), [state.connectionEpoch, state.status, state.activeSessionId,
    state.attachingSessionId, state.sessionAttachmentConfirmed, repository?.path, viewIdentity.current]);
  const livePlanContext = useRef({ boundary: planBoundary, candidate: completedPlan, draftRevision: draftRevision.current,
    modelRevision: modelRevision.current, model: selectedModel.current, pending: objectivePending });
  livePlanContext.current = { boundary: planBoundary, candidate: completedPlan, draftRevision: draftRevision.current,
    modelRevision: modelRevision.current, model: selectedModel.current, pending: objectivePending };
  const preparePlan = async () => {
    const captured = livePlanContext.current;
    if (!captured.candidate || captured.pending || pendingObjectives.current || preparingPlan.current || !shellAvailable()) return;
    const identity = `${captured.candidate.sessionId}:${captured.candidate.runId}`;
    if (preparedPlan === identity) return;
    const current = () => {
      const latest = livePlanContext.current;
      return latest.boundary === captured.boundary && latest.candidate?.runId === captured.candidate?.runId &&
        latest.draftRevision === captured.draftRevision && latest.modelRevision === captured.modelRevision &&
        latest.model === captured.model && !latest.pending && !pendingObjectives.current;
    };
    preparingPlan.current = true;
    // A pre-stage read may still be waiting for the shell; it cannot describe the new defaults.
    defaultsReadGeneration.current++;
    setPlanPreparing(true);
    setPlanPreparationError(null);
    modelRecovery.cancel();
    let modeSaved = false;
    try {
      // Existing desktop semantics: stage Build for the next run, without starting one.
      await localConfigClient.setRunMode({ type: "Build" });
      modeSaved = true;
      if (!current()) return;
      await refreshRunDefaults(current, (defaults) => {
        if (defaults.mode.type !== "Build") {
          setPlanPreparationError("Build mode changed before preparation finished. Your draft was kept; try again.");
          return;
        }
        changeComposerDraft(liveDraft.current.length ? `Implement this plan.\n\n${liveDraft.current}` : "Implement this plan.");
        setPreparedPlan(identity);
        setComposerFocusRequested(true);
      });
    } catch {
      if (current()) setPlanPreparationError(modeSaved
        ? "Build mode was saved, but its settings could not be refreshed. Your draft was kept; try again."
        : "Could not prepare Build mode. Your draft was kept; try again.");
    } finally {
      preparingPlan.current = false;
      setPlanPreparing(false);
    }
  };
  const cancel = useCallback(() => { modelRecovery.cancel(); return cancelNative(); }, [cancelNative, modelRecovery.cancel]);
  const pauseRun = useCallback(() => { modelRecovery.cancel(); return pauseNativeRun(); }, [pauseNativeRun, modelRecovery.cancel]);
  const desktopContext: DesktopContext = {
    view: currentView, appearance, repository: repository?.path ?? null,
    sessionId: state.activeSessionId, runId: state.activeRunId,
    selectedModel: selectedModel.current, mode: runDefaultsLabel?.split(" mode")[0] ?? "Build", draftLength: composerDraft.length,
    title: state.sessions.find((session) => session.id === state.activeSessionId)?.title ?? "New chat",
  };
  const desktopTools = useDesktopTools(connected, state.sessionAttachmentConfirmed ? state.activeSessionId : null, desktopContext, {
    context: () => desktopContext,
    navigate: (view) => {
      if (!NAV_GROUPS.some((group) => group.views.some(([candidate]) => candidate === view))) throw new Error("Unknown workspace page.");
      selectView(view as DesktopView);
    },
    focusComposer: () => { selectView("sessions"); setComposerFocusRequested(true); },
    draft: () => liveDraft.current,
    setDraft: (text, expected) => {
      if (liveDraft.current !== expected) throw new Error("Your draft changed. Read it again before preparing a replacement.");
      changeComposerDraft(text);
    },
    setAppearance: (value) => {
      if (value !== "system" && value !== "light" && value !== "dark") throw new Error("Unknown appearance.");
      setAppearance(value);
    },
    searchChats: async (query) => {
      if (!transport?.searchSessions) throw new Error("Chat search is unavailable.");
      return transport.searchSessions(query);
    },
    listModels: () => localConfigClient.listModels(),
    listProviders: () => localConfigClient.listProviders(),
    listSkills: async () => { if (!knowledge) throw new Error("Skills are unavailable."); return knowledge.listSkills(); },
    listDocuments: async () => { if (!knowledge) throw new Error("Documents are unavailable."); return knowledge.listDocuments(); },
    listMemory: async () => { if (!knowledge) throw new Error("Memory is unavailable."); return knowledge.listMemories(); },
    openFile: openSourceFile,
  });
  useEffect(() => { agent.setState(desktopContext); }, [agent, currentView, appearance, repository?.path, state.activeSessionId, state.activeRunId, runDefaultsLabel, composerDraft.length]);
  /** A failure card's Retry: the same objective, a new run. */
  const retryObjective = useCallback(
    (objective: string) => void sendObjective(objective),
    [sendObjective],
  );
  const reject = useCallback(
    (approvalId: string) => resolveApproval(approvalId, "reject"),
    [resolveApproval],
  );

  const knowledgeScope = useMemo(
    () => ({ epoch: state.connectionEpoch, session: state.activeSessionId }),
    [state.connectionEpoch, state.activeSessionId],
  );
  const liveScope = useRef(knowledgeScope);
  liveScope.current = knowledgeScope;
  const scopedRead = useCallback(
    <T,>(
      fetcher: (() => Promise<T[]>) | undefined,
      commands: readonly string[],
      set: React.Dispatch<React.SetStateAction<Loaded<T>>>,
    ) => {
      const startedUnder = liveScope.current;
      return read(fetcher, commands, set, () => liveScope.current === startedUnder);
    },
    [],
  );

  const loadSkills = useCallback(
    () =>
      scopedRead(knowledge && (() => knowledge.listSkills()), REQUIRED_COMMANDS.skills, setSkills),
    [knowledge, scopedRead],
  );
  const loadMemories = useCallback(
    () =>
      scopedRead(
        knowledge && (() => knowledge.listMemories()),
        REQUIRED_COMMANDS.memories,
        setMemories,
      ),
    [knowledge, scopedRead],
  );
  const loadLearnings = useCallback(
    () =>
      scopedRead(
        knowledge && (() => knowledge.listLearnings()),
        REQUIRED_COMMANDS.learnings,
        setLearnings,
      ),
    [knowledge, scopedRead],
  );
  const loadDocs = useCallback(
    () =>
      scopedRead(knowledge && (() => knowledge.listDocuments()), REQUIRED_COMMANDS.docs, setDocs),
    [knowledge, scopedRead],
  );
  const loadPlugins = useCallback(
    () =>
      read(knowledge && (() => knowledge.listUiPlugins()), REQUIRED_COMMANDS.plugins, setPlugins),
    [knowledge],
  );

  /**
   * The post-boot first-run gate — the desktop's
   * `apply_post_boot_onboard_gate` (`crates/cli/src/tui.rs`).
   *
   * It runs ONCE, reads the three setup conditions from the shell, and opens
   * the setup surface only when one of them is PROVEN to block a run. Three
   * things it deliberately does not do:
   *
   * - It does not run outside the Tauri shell. There is no `models.toml` to
   *   read from a browser tab, and opening a setup wizard there would assert
   *   something about files this build never opened.
   * - It does not open on a failed read. `shouldOpenOnboarding` sees only the
   *   answers; a rejection leaves the app exactly where it was.
   * - It does not override the operator. `skipOnboarding` is their explicit
   *   "stop doing this", and `chosenView` covers the case where they navigated
   *   while the read was in flight.
   */
  useEffect(() => {
    if (skipOnboarding || !shellAvailable() || !connected || !modelDiscoveryReady) {
      return;
    }
    let cancelled = false;
    void readOnboardingStatus()
      .then((status) => {
        if (!cancelled && !chosenView.current && shouldOpenOnboarding(status)) {
          setCurrentView("onboarding");
        }
      })
      // We could not find out whether setup is needed, which is not the same
      // as finding out that it is. Say nothing rather than nag.
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
    // Mount-only: this is a boot decision, not a subscription. `skipOnboarding`
    // is read at mount; toggling it later must not re-open the surface.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [skipOnboarding, connected, modelDiscoveryReady]);

  /**
   * The repository selection and the council roster, read from the shell.
   *
   * Both are local configuration, so they are readable whether or not a daemon
   * is up. A read that FAILS leaves `repository` at `null` only when the shell
   * itself answered "none selected"; a thrown error leaves it `undefined`, and
   * the Repository panel — which does its own read and holds its own three-way
   * status — is where that distinction is shown to the operator.
   */
  useEffect(() => {
    let cancelled = false;
    if (!transport?.currentRepository) {
      return;
    }
    void transport
      .currentRepository()
      .then((selection) => {
        if (!cancelled) {
          setRepository(selection);
        }
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [transport]);

  useEffect(() => {
    let cancelled = false;
    if (!transport?.listCouncils) {
      return;
    }
    void transport
      .listCouncils()
      .then((councils) => {
        if (!cancelled) {
          setCouncilNames(councils.map((council) => council.name));
        }
      })
      // A failure leaves the name list `undefined` — "we do not know" — so the
      // run panel offers no suggestions rather than an empty roster that would
      // read as "you have configured none".
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [transport]);

  const knownScope = useRef(knowledgeScope);
  useEffect(() => {
    if (knownScope.current === knowledgeScope) {
      return;
    }
    knownScope.current = knowledgeScope;
    setSkills(unloaded);
    setMemories(unloaded);
    setLearnings(unloaded);
    setDocs(unloaded);
    setDocsNotice(null);
    setMemoryNotice(null);
  }, [knowledgeScope]);

  // Each surface reads once, the first time it is opened. An unavailable
  // surface is not retried automatically — its Refresh button is the retry,
  // and only exists when there is a transport that could answer.
  useEffect(() => {
    if (currentView === "skills" && skills.status === "unloaded") {
      void loadSkills();
    } else if (currentView === "memory") {
      if (memories.status === "unloaded") {
        void loadMemories();
      }
      if (learnings.status === "unloaded") {
        void loadLearnings();
      }
    } else if (currentView === "docs" && docs.status === "unloaded") {
      void loadDocs();
    } else if (currentView === "plugins" && plugins.status === "unloaded") {
      void loadPlugins();
    }
  }, [
    currentView,
    skills.status,
    memories.status,
    learnings.status,
    docs.status,
    plugins.status,
    loadSkills,
    loadMemories,
    loadLearnings,
    loadDocs,
    loadPlugins,
  ]);

  // ⌘K / Ctrl-K anywhere; `/` only when the focus is not a text field, so it
  // still types a slash in the composer.
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.isComposing || event.defaultPrevented) return;
      if ((event.metaKey || event.ctrlKey) && blockingModalOpen()) return;
      if ((event.metaKey || event.ctrlKey) && !event.altKey && !event.shiftKey && event.key.toLowerCase() === "n") {
        event.preventDefault();
        startNewChat();
        return;
      }
      if ((event.metaKey || event.ctrlKey) && !event.altKey && event.key.toLowerCase() === "b") {
        event.preventDefault();
        toggleSidebar();
        return;
      }
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        openPalette();
        return;
      }
      if (event.key === "Escape") {
        // Escape closes the TOPMOST thing, and only that. It used to fall
        // straight through to `goBack`, so dismissing the cancel dialog also
        // navigated the view away underneath it, and pressing Escape over a
        // modal with no handler of its own left a destructive dialog floating
        // on a screen the operator had just left.
        //
        // `paletteOpen` and the rest are read here, not inside a setter's
        // updater — StrictMode double-invokes updaters and `goBack` pops
        // history, which is how Escape once walked back two views at a time.
        if (shortcutsOpen) {
          setShortcutsOpen(false);
          return;
        }
        if (paletteOpen) {
          setPaletteOpen(false);
          return;
        }
        if (cancelPending) {
          // `ConfirmCancel` has its own Escape handler and no
          // `stopPropagation`, so both fired: it dismissed AND the view moved.
          setCancelPending(false);
          return;
        }
        if (currentView === "council" && buildingCouncil) {
          setBuildingCouncil(false);
          return;
        }
        // Editing text is not a navigation gesture. Escape in a textarea used
        // to leave the view mid-edit and lose what was typed.
        const target = event.target as HTMLElement | null;
        if (
          target instanceof HTMLInputElement ||
          target instanceof HTMLTextAreaElement ||
          target?.isContentEditable === true
        ) {
          return;
        }
        goBack();
        return;
      }
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target?.isContentEditable === true;
      if (event.key === "/" && !typing) {
        event.preventDefault();
        openPalette();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [paletteOpen, goBack, cancelPending, currentView, buildingCouncil, shortcutsOpen, openPalette, toggleSidebar, startNewChat]);

  const handleNavigateInbox = async (deepLink: InboxDeepLink, entry: InboxEntry) => {
    const epoch = liveScope.current.epoch;
    const originView = viewIdentity.current;
    if (deepLink.type === "Session" || deepLink.type === "Run") {
      if (!await selectSession(deepLink.session_id)) throw new Error("Could not open this session. Retry after reconnecting.");
      if (liveScope.current.epoch !== epoch || viewIdentity.current !== originView) return;
      setFocusDecision(null);
      setCurrentView("sessions");
    } else if (deepLink.type === "Approval" || deepLink.type === "Question") {
      const id = deepLink.type === "Approval" ? deepLink.approval_id : deepLink.question_id;
      const sessionId = entry.source.session_id;
      if (sessionId) {
        if (!await selectSession(sessionId)) throw new Error("Could not open the session that owns this request. Retry after reconnecting.");
      } else if (!state.transcript.some((item) => deepLink.type === "Approval" ? item.approvalId === id : item.questionId === id)) {
        throw new Error("This inbox entry does not identify its session. Open the owning session from the session library to review the request.");
      }
      if (liveScope.current.epoch !== epoch || viewIdentity.current !== originView) return;
      setFocusDecision(`${deepLink.type}:${id}`);
      setCurrentView("sessions");
    } else if (deepLink.type === "Workflow") {
      setCurrentView("workflow");
    } else if (deepLink.type === "Plugin") {
      setCurrentView("plugins");
    } else if (deepLink.type === "Repository") {
      setCurrentView("repository");
    }
  };

  /** Run a mutation, surface its outcome sentence, then re-read the list. */
  const applyMutation = async (
    run: () => Promise<string | void>,
    setNotice: (notice: string | null) => void,
    reload: () => Promise<void>,
    stillCurrent: () => boolean = () => true,
  ) => {
    try {
      const outcome = await run();
      if (!stillCurrent()) return;
      setNotice(typeof outcome === "string" && outcome.length > 0 ? outcome : null);
    } catch (error) {
      // A refused mutation is an outcome, not an absence: say what happened
      // and leave the list showing what the daemon last actually reported.
      if (!stillCurrent()) return;
      setNotice(describe(error));
    }
    await reload();
  };
  const applyScopedMutation = (...args: Parameters<typeof applyMutation>) => {
    const scope = liveScope.current;
    return applyMutation(args[0], args[1], args[2], () => liveScope.current === scope);
  };

  const paletteEntries: PaletteEntry[] = [
    { id: "action:new-session", title: "New chat", description: state.isRunning ? "finish or cancel the current run first" : "start a separate conversation and keep your unsent draft", key: "⌘N / Ctrl-N", group: "Run" },
    {
      id: "view:onboarding",
      title: "Get started",
      description:
        "check your detected models and workspace",
      key: "—",
      group: "Setup",
    },
    {
      id: "view:repository",
      title: "Repository",
      description: "choose the project to work on",
      key: "—",
      group: "Setup",
    },
    {
      id: "view:sessions",
      title: "Chat",
      description: "return to your conversation",
      key: "—",
      group: "Run",
    },
    {
      id: "view:inbox",
      title: "Inbox",
      description: "review updates and requests waiting for you",
      key: "—",
      group: "Run",
    },
    {
      id: "view:analytics",
      title: "Usage",
      description: "review usage and measured results",
      key: "—",
      group: "Run",
    },
    {
      id: "view:context",
      title: "Context",
      description: "view detailed token usage breakdown for the active run",
      key: "—",
      group: "Run",
    },
    {
      id: "view:library",
      title: "All chats",
      description: "search every session and rename, pin, archive, or export one",
      key: "—",
      group: "Run",
    },
    ...(canControlRun
      ? [
          {
            id: "action:steer",
            title: "Steer run",
            description: "redirect the live run without killing it",
            key: "—",
            group: "Run",
          },
        ]
      : []),
    ...(canControlRun
      ? [
          {
            id: "action:cancel",
            title: "Cancel run",
            description: "confirm, then cancel the selected run",
            key: "—",
            group: "Run",
          },
        ]
      : []),
    {
      id: "action:shortcuts",
      title: "Keyboard shortcuts",
      description: "every key the desktop app answers to, in one card",
      key: "—",
      group: "Session",
    },
    {
      id: "view:workflow",
      title: "Workflows",
      description: "start a workflow and follow its progress",
      key: "—",
      group: "Workflow",
    },
    {
      id: "view:board",
      title: "Task board",
      description: "create, assign, and move repository backlog tasks in Kanban columns",
      key: "—",
      group: "Workflow",
    },
    {
      id: "view:blackboard",
      title: "Shared results",
      description: "inspect the workflow blackboard, shared evidence and open questions",
      key: "—",
      group: "Workflow",
    },
    {
      id: "view:docs",
      title: "Documents",
      description: `create, edit and publish workspace documents${knowledge ? "" : " (not in this build)"}`,
      key: "—",
      group: "Workspace",
    },
    {
      id: "view:skills",
      title: "Skills",
      description: `inspect registered skills and their permissions${knowledge ? "" : " (not in this build)"}`,
      key: "—",
      group: "Workspace",
    },
    {
      id: "view:memory",
      title: "Memory",
      description: `browse curated memories and their provenance${knowledge ? "" : " (not in this build)"}`,
      key: "—",
      group: "Workspace",
    },
    {
      id: "view:plugins",
      title: "Extensions",
      description: `manage verified workspace extensions${knowledge ? "" : " (not in this build)"}`,
      key: "—",
      group: "Workspace",
    },
    {
      id: "view:council",
      title: "Councils",
      description: "list, run, and manage persisted multi-model councils",
      key: "—",
      group: "Models",
    },
    {
      id: "view:councilResults",
      title: "Council results",
      description: "open durable council outcomes",
      key: "—",
      group: "Models",
    },
    { id: "action:sidebar", title: sidebarVisible ? "Hide sidebar" : "Show sidebar", description: "make more room for the current workspace", key: "⌘B / Ctrl-B", group: "Workspace" },
    ...(["system", "light", "dark"] as const).map((theme) => ({ id: `appearance:${theme}`, title: `Appearance: ${theme}`, description: theme === "system" ? "follow your operating system appearance" : `use the ${theme} appearance`, key: "—", group: "Appearance" })),
  ];

  const runPaletteCommand = (id: string) => {
    setPaletteOpen(false);
    if (id === "action:new-session") { startNewChat(); return; }
    if (id === "action:sidebar") { toggleSidebar(); return; }
    if (id === "appearance:system" || id === "appearance:light" || id === "appearance:dark") { setAppearance(id.slice(11) as "system" | "light" | "dark"); return; }
    if (id === "action:steer") {
      setCurrentView("sessions");
      setSteeringOpen(true);
      return;
    }
    if (id === "action:shortcuts") {
      setShortcutsOpen(true);
      return;
    }
    if (id === "action:cancel") {
      // The palette asks the same question the composer button does. There is
      // no second path that cancels a run without confirmation.
      setCurrentView("sessions");
      setCancelPending(true);
      return;
    }
    // The palette is a front door to the existing views, never a second code
    // path: it only ever selects a view this build actually mounts.
    const view = id.startsWith("view:") ? (id.slice("view:".length) as DesktopView) : null;
    if (view) {
      selectView(view);
    }
  };

  useEffect(() => {
    if (!composerFocusRequested) return;
    // Preparation can commit its draft before the mode-read promise unwinds.
    if (planPreparing) return;
    if (currentView !== "sessions" || paletteOpen || shortcutsOpen || blockingModalOpen()) {
      setComposerFocusRequested(false);
      return;
    }
    // Focus after the committed view and the palette's passive cleanup have
    // restored their DOM. Native menu actions must not wait for an animation
    // frame, which WebKit can defer while the window is being activated.
    document.querySelector<HTMLTextAreaElement>(".workspace-composer textarea:not(:disabled)")?.focus();
    setComposerFocusRequested(false);
  }, [composerFocusRequested, currentView, paletteOpen, shortcutsOpen, planPreparing]);

  useNativeDesktopActions((action) => {
    if (blockingModalOpen()) return;
    if (action === "choose-repository" || action === "settings" || action === "focus-composer") {
      setPaletteOpen(false);
      setShortcutsOpen(false);
    }
    if (action === "command-palette") openPalette();
    else if (action === "new-session") startNewChat();
    else if (action === "toggle-sidebar") toggleSidebar();
    else if (action === "choose-repository") selectView("repository");
    else if (action === "settings") selectView("models");
    else if (action === "focus-composer") {
      selectView("sessions");
      setComposerFocusRequested(true);
    }
  });

  return (
    <div className="workspace-shell" data-sidebar={sidebarVisible ? "visible" : "hidden"}>
      <Navigation
        hidden={!sidebarVisible}
        modal={compact && sidebarVisible && !paletteOpen && !shortcutsOpen}
        onClose={closeSidebar}
        restoreFocusFallback={sidebarToggleRef}
        sessions={state.sessions}
        activeSessionId={state.activeSessionId}
        onSelectSession={selectSessionFromNav}
        connectionStatus={connectionPresentation}
        statusDetail={state.detail}
        connectionInfo={state.info}
        currentView={currentView}
        onSelectView={selectView}
        unreadInboxCount={state.unreadInboxCount}
        onOpenPalette={openPalette}
        onNewSession={startNewChat}
        newSessionDisabled={state.isRunning || state.attachingSessionId !== null}
        unavailableViews={knowledge ? undefined : KNOWLEDGE_VIEWS}
      />
      {sidebarVisible && <button className="workspace-sidebar-scrim" type="button" aria-label="Close sidebar" onClick={toggleSidebar} />}

      <main className="workspace-main" id="workspace-main" inert={compact && sidebarVisible}>
        <WorkspaceBar
          view={currentView}
          sidebarToggleRef={sidebarToggleRef}
          sessionTitle={state.sessions.find((session) => session.id === state.activeSessionId)?.title}
          sessionId={connected ? state.activeSessionId : null}
          repository={repository?.path}
          connectionStatus={connectionPresentation}
          sidebarVisible={sidebarVisible}
          onToggleSidebar={toggleSidebar}
          appearance={appearance}
          onAppearanceChange={setAppearance}
          onOpenPalette={openPalette}
          onOpenRepository={() => selectView("repository")}
          onNewSession={startNewChat}
          newSessionDisabled={state.isRunning || state.attachingSessionId !== null}
        />
        {/*
          The connection state lives here, across every view, not in a sidebar
          corner: "not connected" changes what everything below can be trusted
          to mean. The banner also offers to START the daemon, which the shell
          never could before — every first launch used to be a raw socket error
          and a reconnect loop that could not succeed.
        */}
        <ConnectionBanner
          status={connectionPresentation}
          detail={state.detail}
          hasTransport={transport !== null}
          canStart={Boolean(transport?.startDaemon)}
          onStart={startWorkspace}
          // The rejection is already on screen: a failed attempt dispatches
          // `connect-failed`, which is what this banner renders. Catching it
          // keeps the promise from surfacing as an unhandled rejection — which
          // would reach global error reporting and fail UI tests — while
          // telling the operator nothing they are not already being told.
          onRetry={() => {
            void reconnect().catch(() => undefined);
          }}
          launchStatus={launchStatus}
        />

        {state.error && (
          // Hoisted above the per-view blocks: `command-failed` fires from a
          // dozen call sites reachable from EVERY view (inbox acks, analytics
          // queries, approvals), and the banner used to render only inside
          // the Sessions view — an error earned elsewhere was invisible.
          <div
            role="alert"
            style={{
              padding: "8px 24px",
              background: "var(--cody-danger-bg)",
              borderBottom: "1px solid var(--cody-danger)",
              color: "var(--cody-danger-text)",
              fontSize: 12,
              display: "flex",
              justifyContent: "space-between",
              alignItems: "center",
              gap: 12,
            }}
          >
            <span>{state.error}</span>
            <button
              aria-label="Dismiss error"
              onClick={dismissError}
              style={{
                background: "transparent",
                border: "none",
                color: "var(--cody-danger-text)",
                cursor: "pointer",
                fontSize: 14,
                lineHeight: 1,
              }}
            >
              ×
            </button>
          </div>
        )}

        {/*
          Where you are and the way back, on every view but the working
          surface. Escape has always walked the view history; this is the
          first time the screen says so.
        */}
        {currentView !== "sessions" && <ViewBar view={currentView} onBack={goBack} />}

        <React.Suspense fallback={<div className="workspace-loading" role="status">Opening workspace…</div>}>
        {currentView === "sessions" && (
          <>
            {modelRecovery.status && <div role="status" style={{ padding: "10px 24px", color: "var(--cody-text-muted)", fontSize: 13 }}>{modelRecovery.status}</div>}
            <Transcript
              recoveredRuns={modelRecovery.recoveredRuns}
              key={state.activeSessionId}
              commandActions={commandActions}
              focusDecision={focusDecision}
              followLatestRequest={followLatestRequest}
              onReadArtifact={connected && transport?.readArtifact ? readArtifact : undefined}
              onSaveArtifact={connected && transport?.saveArtifact ? saveArtifact : undefined}
              onOpenFile={connected && transport?.openWorkspace && repository?.path ? openSourceFile : undefined}
              items={state.transcript}
              connectionStatus={connectionPresentation}
              statusDetail={state.detail}
              onApprove={connected ? approve : undefined}
              onReject={connected ? reject : undefined}
              activity={state.activity}
              onRetry={connected && !state.isRunning ? retryObjective : undefined}
              onOpenView={selectView}
              onResolveQuestion={connected ? answerQuestion : undefined}
            />
            {steeringOpen && (
              <Steering
                runId={state.activeRunId}
                events={state.durableEvents}
                onSteer={steer}
                canSteer={canControlRun}
                unavailableDetail={
                  connected
                    ? "The daemon has not named a run id for this session, so there is nothing to steer."
                    : state.detail
                }
                onClose={() => setSteeringOpen(false)}
              />
            )}
            {queueVisible && (
              <PromptQueue
                prompts={state.pendingPrompts}
                canMutate={canQueuePrompts}
                unavailableDetail={
                  connected
                    ? state.attachingSessionId !== null
                      ? `Session ${state.attachingSessionId} is still attaching, so queue changes are temporarily unavailable.`
                      : !state.sessionAttachmentConfirmed
                        ? "The current connection has not confirmed a session attachment, so queue changes are unavailable. Reopen the session or start a new one."
                      : state.activeSessionId === null
                        ? "No session is attached, so there is no queue to change — open or start a session first."
                        : "This build's bridge does not offer the prompt-queue commands."
                    : state.detail
                }
                error={state.promptQueueError}
                actions={promptQueueActions}
                onClose={() => {
                  setQueueOpen(false);
                  setQueueDismissed(true);
                }}
              />
            )}
            {completedPlan && !objectivePending && shellAvailable() && preparedPlan !== `${completedPlan.sessionId}:${completedPlan.runId}` && (
              <PlanHandoff preparing={planPreparing} error={planPreparationError} onPrepare={() => void preparePlan()} />
            )}
            <Composer
              statusLabel={runDefaultsLabel}
              onOpenModels={() => selectView("models")}
              onOpenMode={() => selectView("mode")}
              draft={composerDraft}
              onDraftChange={changeComposerDraft}
              onDraftAccepted={acceptComposerDraft}
              onSend={sendObjective}
              activity={state.activity}
              usage={state.usage}
              onQueue={queuePrompt}
              canQueue={canQueuePrompts}
              queuedCount={state.pendingPrompts.length}
              queueOpen={queueVisible}
              onToggleQueue={
                connected
                  ? () => {
                      setQueueDismissed(false);
                      setQueueOpen(!queueVisible);
                    }
                  : undefined
              }
              onRequestCancel={() => setCancelPending(true)}
              isRunning={state.isRunning}
              disabled={!connected || state.attachingSessionId !== null || planPreparing}
              canCancel={canControlRun}
              canSteer={canControlRun}
              steeringOpen={steeringOpen}
              onToggleSteering={() => setSteeringOpen((open) => !open)}
              lifecycle={lifecycle}
              onPause={() => void pauseRun()}
              onResume={() => void resumeRun()}
            />
          </>
        )}

        {atStake && state.activeRunId !== null && (
          <ConfirmCancel
            runId={state.activeRunId}
            objective={atStake.objective}
            startedAt={atStake.startedAt}
            onConfirm={() => {
              setCancelPending(false);
              void cancel();
            }}
            onDismiss={() => setCancelPending(false)}
          />
        )}

        {currentView === "inbox" && (
          <InboxView
            entries={state.inbox}
            onAcknowledge={acknowledgeInbox}
            onDismiss={dismissInbox}
            connected={connected}
            commandActions={commandActions}
            onNavigate={handleNavigateInbox}
            onRefresh={() => void loadInbox()}
            unavailable={state.inboxStatus === "unavailable" ? state.inboxDetail : null}
          />
        )}

        {currentView === "analytics" && (
          <AnalyticsDashboard
            connectionEpoch={state.connectionEpoch}
            onQueryAnalytics={connected ? queryAnalytics : undefined}
            onExportAnalytics={connected ? exportAnalytics : undefined}
            onSaveArtifact={connected && transport?.saveArtifact ? saveArtifact : undefined}
          />
        )}

        {currentView === "library" && (
          <SessionLibrary
            transport={transport}
            unavailable={connected ? null : state.detail}
            onOpenSession={(sessionId) => {
              setCurrentView("sessions");
              void selectSession(sessionId);
            }}
          />
        )}

        {currentView === "workflow" && (
          <WorkflowView
            key={repository?.path ?? "no-repository"}
            transport={transport}
            repository={repository?.path ?? null}
            onOpenRepository={() => selectView("repository")}
            launchMemory={workflowLaunchMemory}
            connectionEpoch={state.connectionEpoch}
            unavailable={connected ? null : state.detail}
            onOpenBlackboard={(workflowRunId) => {
              setBlackboardRunId(workflowRunId);
              setCurrentView("blackboard");
            }}
          />
        )}

        {currentView === "board" && (
          <KanbanView
            onOpenRepository={() => selectView("repository")}
            transport={transport}
            unavailable={transport ? null : state.detail}
            connectionEpoch={state.connectionEpoch}
          />
        )}

        {currentView === "blackboard" && (
          <BlackboardView
            transport={transport}
            workflowRunId={blackboardRunId}
            connectionEpoch={state.connectionEpoch}
            unavailable={transport ? null : state.detail}
          />
        )}

        {currentView === "context" && (
          <ContextView events={state.durableEvents} activeRunId={state.activeRunId} />
        )}

        {/* The code graph. Daemon-backed and PAGED: the panel sends its own
            limit on every read and renders the daemon's pre-limit totals, so a
            cut page never reads as the whole graph. */}
        {currentView === "edges" && (
          <EdgesView transport={transport} unavailable={transport ? null : state.detail}
            onOpenFile={connected && transport?.openWorkspace && repository?.path ? (file, line) => openSourceFile(file, line, null) : undefined} />
        )}

        {/* Backtrack reads the attached session's own ledger — the same durable
            events the transcript is built from — so its checkpoint list is
            daemon state, not a client-side history of what this window did. */}
        {currentView === "backtrack" && (
          <BacktrackView
            events={state.durableEvents}
            activeSessionId={state.activeSessionId}
            transport={transport}
            unavailable={transport ? null : state.detail}
            onOpenSession={(sessionId) => {
              setCurrentView("sessions");
              void selectSession(sessionId);
            }}
          />
        )}

        {/* First-run setup. Also local config — it reads models.toml,
            auth.json and the repository preference, never the daemon — and it
            routes to the four surfaces below rather than duplicating them. */}
        {currentView === "onboarding" && (
          <Onboarding
            onOpen={selectView}
            connected={connected}
            skipped={skipOnboarding}
            onSkip={(skipped) => {
              setOnboardingSkipped(skipped);
              setSkipOnboarding(skipped);
            }}
          />
        )}

        {/* Local-config surfaces. Each fetches its own data and renders its own
            unavailable state, so they take no props — see localConfig.ts. */}
        {currentView === "models" && (
          <ModelPicker
            onModelPinned={(model) => {
              modelRecovery.cancel();
              modelRevision.current++;
              selectedModel.current = model;
            }}
            initialProvider={pendingProvider}
            onInitialProviderUsed={() => setPendingProvider(null)}
          />
        )}
        {currentView === "providers" && (
          <ProviderPicker
            repositoryPath={repository?.path}
            onConnected={() => selectView("models")}
            // The page had no `onSelect`, so every click optional-chained into
            // nothing and the list looked inert. A chosen provider now opens the
            // add-model flow already on it, which is where its credential and
            // model details are entered.
            onSelect={(provider) => {
              setPendingProvider(provider);
              selectView("models");
            }}
          />
        )}
        {currentView === "keys" && <ApiKeys />}
        {currentView === "mode" && <ModePicker />}
        {currentView === "agentTools" && <AgentToolsView sessionId={state.sessionAttachmentConfirmed ? state.activeSessionId : null} connected={connected} error={desktopTools.error} onCompose={(text) => {
          if (!liveDraft.current) changeComposerDraft(text);
          selectView("sessions");
          setComposerFocusRequested(true);
        }} />}

        {currentView === "skills" && (
          <SkillsView skills={skills} onRefresh={knowledge && (() => void loadSkills())} />
        )}

        {currentView === "memory" && (
          <MemoryView
            memories={memories}
            learnings={learnings}
            notice={memoryNotice}
            onRefresh={
              knowledge &&
              (() => {
                setMemoryNotice(null);
                void loadMemories();
                void loadLearnings();
              })
            }
            onCorrectMemory={
              knowledge &&
              ((memoryId, statement) =>
                void applyScopedMutation(
                  () => knowledge.correctMemory(memoryId, statement),
                  setMemoryNotice,
                  loadMemories,
                ))
            }
            onForgetMemory={
              knowledge &&
              ((memoryId) =>
                void applyScopedMutation(
                  () => knowledge.forgetMemory(memoryId),
                  setMemoryNotice,
                  loadMemories,
                ))
            }
            onMutateLearning={
              knowledge &&
              ((id: string, revision: number, mutation: LearningMutation) =>
                void applyScopedMutation(
                  () => knowledge.mutateLearning(id, revision, mutation),
                  setMemoryNotice,
                  loadLearnings,
                ))
            }
          />
        )}

        {currentView === "docs" && (
          <DocsView
            docs={docs}
            notice={docsNotice}
            onRefresh={
              knowledge &&
              (() => {
                setDocsNotice(null);
                void loadDocs();
              })
            }
            onCreateDocument={
              knowledge &&
              ((title) =>
                void applyScopedMutation(
                  async () => {
                    await knowledge.createDocument(title);
                    return "document created";
                  },
                  setDocsNotice,
                  loadDocs,
                ))
            }
            onReplaceBlock={
              knowledge &&
              ((documentId, blockId, original, replacement) =>
                void applyScopedMutation(
                  async () => {
                    await knowledge.replaceDocumentBlock(
                      documentId,
                      blockId,
                      original,
                      replacement,
                    );
                    return "block replaced";
                  },
                  setDocsNotice,
                  loadDocs,
                ))
            }
            onDeleteBlock={
              knowledge &&
              ((documentId, blockId) =>
                void applyScopedMutation(
                  async () => {
                    await knowledge.deleteDocumentBlock(documentId, blockId);
                    return "block deleted";
                  },
                  setDocsNotice,
                  loadDocs,
                ))
            }
            onPublish={
              knowledge &&
              ((documentId: string, target: PublishTarget) =>
                void applyScopedMutation(
                  async () => {
                    const plan = await knowledge.publishDocument(documentId, target);
                    const files = plan.changed_files.length;
                    return (
                      `publish parked for approval: ${plan.git_action} to ${plan.target} ` +
                      `(${files} file${files === 1 ? "" : "s"}) — approve it from the Inbox or the session`
                    );
                  },
                  setDocsNotice,
                  loadDocs,
                ))
            }
          />
        )}

        {currentView === "plugins" && (
          <PluginsView
            plugins={plugins}
            notice={pluginNotice}
            onRefresh={
              knowledge &&
              (() => {
                setPluginNotice(null);
                void loadPlugins();
              })
            }
            onSmokeTest={
              knowledge &&
              ((pluginId) =>
                void applyMutation(
                  async () => {
                    await knowledge.smokeTestUiPlugin(pluginId);
                    return "smoke test requested";
                  },
                  setPluginNotice,
                  loadPlugins,
                ))
            }
            onEnable={
              knowledge &&
              ((pluginId, scope) =>
                void applyMutation(
                  async () => {
                    await knowledge.enableUiPlugin(pluginId, scope);
                    return `enable requested at ${scope} scope`;
                  },
                  setPluginNotice,
                  loadPlugins,
                ))
            }
            onApprove={
              knowledge &&
              ((pluginId, receipt) =>
                void applyMutation(
                  async () => {
                    await knowledge.approveUiPluginUpdate(pluginId, receipt);
                    return "update approved";
                  },
                  setPluginNotice,
                  loadPlugins,
                ))
            }
            onReject={
              knowledge &&
              ((pluginId, receipt) =>
                void applyMutation(
                  async () => {
                    await knowledge.rejectUiPluginUpdate(pluginId, receipt);
                    return "update rejected";
                  },
                  setPluginNotice,
                  loadPlugins,
                ))
            }
            onRevoke={
              knowledge &&
              ((pluginId) =>
                void applyMutation(
                  async () => {
                    await knowledge.revokeUiPlugin(pluginId);
                    return "plugin revoked";
                  },
                  setPluginNotice,
                  loadPlugins,
                ))
            }
          />
        )}

        {currentView === "repository" && (
          <RepoPicker
            connected={connected}
            // The prop and its button already existed; nothing ever passed it,
            // so "Reconnect now" could not render and rebinding the repository
            // on demand was unreachable.
            onReconnect={reconnect}
            onLoad={async () => {
              if (!transport?.currentRepository) {
                throw new Error(
                  "the shell does not expose `current_repository`; run the desktop app rather than a browser tab",
                );
              }
              const selection = await transport.currentRepository();
              setRepository(selection);
              return selection;
            }}
            onPick={async () => {
              if (!transport?.pickRepository) {
                throw new Error(
                  "the shell does not expose `pick_repository`; run the desktop app rather than a browser tab",
                );
              }
              const selection = await transport.pickRepository();
              if (selection !== null) {
                setRepository(selection);
              }
              return selection;
            }}
            onSetPath={
              transport?.setRepository &&
              (async (path) => {
                const selection = await transport.setRepository!(path);
                setRepository(selection);
                return selection;
              })
            }
            onClear={
              transport?.clearRepository &&
              (async () => {
                await transport.clearRepository!();
                setRepository(null);
              })
            }
          />
        )}

        {currentView === "council" &&
          (buildingCouncil ? (
            <CouncilBuilder
              draft={councilDraft}
              onDraftChange={setCouncilDraft}
              configuredModels={configuredModelIds}
              onOpenModels={() => selectView("models")}
              onCreate={async (draft) => {
                if (!transport?.createCouncil) {
                  throw new Error(
                    "the shell does not expose `create_council`; run the desktop app rather than a browser tab",
                  );
                }
                return transport.createCouncil(draft);
              }}
              onCancel={() => setBuildingCouncil(false)}
              onCreated={(council) => {
                setCouncilNames((current) =>
                  current === undefined ? [council.name] : [...current, council.name],
                );
                setCouncilDraft(newCouncilDraft());
                setBuildingCouncil(false);
              }}
            />
          ) : (
            <CouncilBrowser
              onLoad={async () => {
                if (!transport?.listCouncils) {
                  throw new Error(
                    "the shell does not expose `list_councils`; run the desktop app rather than a browser tab",
                  );
                }
                const councils = await transport.listCouncils();
                setCouncilNames(councils.map((council) => council.name));
                return councils;
              }}
              onDelete={
                transport?.deleteCouncil &&
                (async (name) => {
                  await transport.deleteCouncil!(name);
                  setCouncilNames((current) =>
                    current?.filter((candidate) => candidate !== name),
                  );
                })
              }
              onCreate={() => { setCouncilDraft(newCouncilDraft()); setBuildingCouncil(true); }}
              onOpenResults={() => {
                councilRun.setCouncil("");
                selectView("councilResults");
              }}
              onRun={(name) => {
                councilRun.setCouncil(name);
                selectView("councilResults");
              }}
            />
          ))}

        {currentView === "councilResults" && (
          <CouncilResults
            runController={councilRun}
            councilNames={councilNames}
            repository={repository === undefined ? undefined : (repository?.path ?? null)}
            onOpenRepository={() => selectView("repository")}
            onLoad={async () => {
              if (!transport?.listCouncilResults) {
                throw new Error(
                  "the shell does not expose `list_council_results`; run the desktop app rather than a browser tab",
                );
              }
              return transport.listCouncilResults();
            }}
            onRun={
              transport?.runCouncil &&
              ((name, objective, onProgress) =>
                transport.runCouncil!(
                  name,
                  objective,
                  // The repository is the operator's selection, never a guess;
                  // passing `null` lets the shell refuse when none is chosen.
                  { repository: repository?.path ?? null, sessionId: state.activeSessionId },
                  onProgress,
                ))
            }
          />
        )}
        </React.Suspense>
      </main>

      {shortcutsOpen && <ShortcutsCard onClose={() => setShortcutsOpen(false)} restoreFocusFallback={sidebarToggleRef} />}
      {paletteOpen && <CommandPalette
        open={true}
        restoreFocusFallback={sidebarToggleRef}
        entries={paletteEntries}
        onRun={runPaletteCommand}
        onClose={() => setPaletteOpen(false)}
      />}

      {connected && state.sessionAttachmentConfirmed && !state.attachingSessionId && state.activeSessionId && transport?.sendRemoteUi &&
        <React.Suspense fallback={null}>
          <RemoteUiHost key={`${state.connectionEpoch}:${state.activeSessionId}`} transport={transport} sessionId={state.activeSessionId} />
        </React.Suspense>}
    </div>
  );
};
