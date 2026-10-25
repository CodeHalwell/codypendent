/**
 * VS Code / Cursor extension entry point for Codypendent (Phase 3 STEP 3.5).
 *
 * This is the ONLY module that imports `vscode`; every protocol/transport concern
 * lives in the pure, unit-tested modules under `src/protocol/` and `src/client.ts`.
 *
 * Responsibilities:
 *   - resolve the daemon socket exactly as the daemon does and open a
 *     {@link DaemonClient} (attach as Approver, reconnect + attach-resume);
 *   - render the session transcript and run state in a side-panel webview from
 *     the daemon's events / catch-up projections;
 *   - raise an editor notification for work that BLOCKS A HUMAN (approvals and
 *     questions only, via `notifications.ts`), whose actions resolve the
 *     approval or focus the parked session;
 *   - push a debounced (>= 300 ms) `IdeContextUpdate` on active-editor,
 *     selection, dirty-buffer, and diagnostics changes;
 *   - show change sets with `vscode.diff`;
 *   - register the three contributed commands.
 *
 * INVARIANT: the extension NEVER executes tools locally. It only observes the
 * session, forwards editor context, and relays the user's approval decisions.
 * It holds no session state beyond the client's connection + last-seen sequence;
 * the webview holds only rolling view state, recovered on reload via attach.
 */
import { createHash } from "node:crypto";
import * as vscode from "vscode";

import { DaemonClient, type ConnectionStatus } from "./client.js";
import { registerEditorActions } from "./editor-actions.js";
import { parseHandoff, validateHandoff, workspaceMatches, type SessionHandoff } from "./handoff.js";
import {
  applyUnifiedPatch,
  isWorkspaceUriPath,
  parseUnifiedDiff,
  patchSourcePath,
  readPatchSource,
  reviewablePatchFiles,
} from "./patch-review.js";
import { resolveRuntimePaths } from "./protocol/discovery.js";
import {
  isMediatedRuntimeWire,
  isUiRuntimeMessage,
  runtimeToWire,
  wireToHost,
  type UiWireMessage,
} from "./remote-ui/wire.js";
import {
  makeNonce,
  renderPanelHtml,
  type TranscriptMessage,
  type WebviewCommandMessage,
} from "./webview/panel.js";
import {
  InboxItemTreeItem,
  InboxStatusBarIndicator,
  InboxTreeDataProvider,
  showInboxQuickPick,
} from "./inbox.js";
import {
  blockingWorkOf,
  BlockingWorkNotifier,
  resolvedWorkKeyOf,
  subscribeBlockingWork,
} from "./notifications.js";
import type {
  ArtifactRef,
  DirtyBufferDigest,
  EditorSelection,
  EventBody,
  IdeContextUpdate,
  InboxDeepLink,
  InboxEntry,
  SessionEvent,
  SessionLifecycleAction as GeneratedSessionLifecycleAction,
  UiWireMessage as GeneratedUiWireMessage,
  Uuid,
} from "@codypendent/protocol";
import type { UiCapabilities, UiRuntimeMessage } from "@codypendent/ui";

// ---------------------------------------------------------------------------
// Wire-type boundary
// ---------------------------------------------------------------------------
//
// Every protocol type this module handles comes from `@codypendent/protocol`,
// which is generated from the Rust JSON schemas — there is deliberately no
// second, hand-written copy of the wire in this extension any more.
//
// Two of the shapes the transcript renderer needs (`ProposedAction`, `Risk`)
// are declared inside the generated `EventBody` module rather than exported
// under their own names, and the generated view is WIDER than the package's
// stable facades (`ProposedAction` in particular gained a `ReadSecret` variant
// the facade does not name). Extracting them straight out of `EventBody` —
// the same idiom `@codypendent/protocol` itself uses for `UiWireMessage` —
// gives the renderer exactly the union the daemon can send, with no cast and
// nothing to drift against. A facade-typed action (e.g. the one carried on a
// `Catchup` projection) is a strict subset and assigns into these directly.

/** `ProposedAction` exactly as it arrives inside a generated `EventBody`. */
type WireProposedAction = Extract<EventBody, { type: "ApprovalRequested" }>["action"];

/** `Risk` exactly as it arrives inside a generated `EventBody`. */
type WireRisk = Extract<EventBody, { type: "ApprovalRequested" }>["risk"];

// `src/remote-ui/wire.ts` still keeps its own `UiWireMessage` view: it is the
// validated, `@codypendent/ui`-typed projection the webview consumes, not a
// copy of the Rust wire (the generated `UiWireMessage` nests schema-generated
// `UiNode`/`UiEvent` trees rather than the SDK's). The bytes are identical, so
// crossing between the two views is a re-view and never a value conversion.
// These two named helpers are the only place that re-view happens.

function asRemoteUiWire(message: GeneratedUiWireMessage): UiWireMessage {
  return message as unknown as UiWireMessage;
}

function asProtocolWire(message: UiWireMessage): GeneratedUiWireMessage {
  return message as unknown as GeneratedUiWireMessage;
}

const IDE_CONTEXT_DEBOUNCE_MS = 300;
const DIFF_SCHEME = "codypendent-diff";

let client: DaemonClient | undefined;
/**
 * The session `client` is attached to. Kept beside `client` so a notification
 * raised for one session can never relay its decision through a replacement
 * client attached to a different one.
 */
let attachedSessionId: Uuid | undefined;
let attachedSocketPath: string | undefined;
let attachedRepository: string | undefined;
let view: vscode.WebviewView | undefined;
let latestWebviewCapabilities: UiCapabilities | undefined;
let currentLibraryQuery = "";
const pendingRemoteUiResyncs = new Map<string, number | undefined>();

export function activate(context: vscode.ExtensionContext): void {
  const output = vscode.window.createOutputChannel("Codypendent");
  context.subscriptions.push(output);

  const inboxTreeProvider = new InboxTreeDataProvider(client);
  context.subscriptions.push(
    vscode.window.registerTreeDataProvider("codypendent.inboxView", inboxTreeProvider),
  );

  const inboxStatusBar = new InboxStatusBarIndicator();
  context.subscriptions.push(inboxStatusBar);

  const refreshInbox = async () => {
    await inboxTreeProvider.refresh();
    inboxStatusBar.update(inboxTreeProvider.unreadCount);
  };

  // The extension's ONLY notification surface for blocking work: approvals and
  // questions raise an editor notification, and its "Open Session" action
  // attaches to (and reveals) the session that is parked. Everything else the
  // daemon emits stays in the transcript, the tree and the status bar — see
  // `notifications.ts` for why over-notifying is treated as a defect.
  // `connect` is a hoisted function declaration in this scope.
  const notifier = new BlockingWorkNotifier({
    resolveApproval: (sessionId, approvalId, decision) => {
      if (client === undefined || attachedSessionId !== sessionId) {
        void vscode.window.showWarningMessage(
          `Codypendent: no longer attached to that session, so the ${decision.toLowerCase()} was not sent. Reopen the session and resolve it there.`,
        );
        return;
      }
      client.resolveApproval(approvalId, decision);
    },
    focusSession: (sessionId) => {
      if (attachedSessionId !== sessionId) {
        connect(sessionId);
      }
      void vscode.commands.executeCommand("codypendent.sessionView.focus");
    },
  });

  // Virtual-document provider backing `vscode.diff` for proposed change sets.
  // Bounded: each PatchProposed adds two entries, and an unbounded map grows
  // for the whole window lifetime — evict oldest beyond the cap (an evicted
  // diff view simply renders empty if reopened later, which is acceptable for
  // a transient preview).
  const MAX_DIFF_ENTRIES = 64;
  const diffContents = new Map<string, string>();
  const diffProvider: vscode.TextDocumentContentProvider = {
    provideTextDocumentContent(uri) {
      return diffContents.get(uri.toString()) ?? "";
    },
  };
  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider(DIFF_SCHEME, diffProvider),
  );

  function rememberDiffContent(key: string, content: string): void {
    diffContents.set(key, content);
    while (diffContents.size > MAX_DIFF_ENTRIES) {
      const oldest = diffContents.keys().next().value;
      if (oldest === undefined) {
        break;
      }
      diffContents.delete(oldest);
    }
  }

  async function showDiff(
    title: string,
    leftLabel: string,
    rightLabel: string,
    left: string,
    right: string,
  ): Promise<void> {
    const stamp = Date.now();
    const leftUri = vscode.Uri.parse(`${DIFF_SCHEME}:/${leftLabel}-${stamp}`);
    const rightUri = vscode.Uri.parse(`${DIFF_SCHEME}:/${rightLabel}-${stamp}`);
    rememberDiffContent(leftUri.toString(), left);
    rememberDiffContent(rightUri.toString(), right);
    await vscode.commands.executeCommand("vscode.diff", leftUri, rightUri, title);
  }

  async function reviewPatch(artifact: ArtifactRef, title: string): Promise<void> {
    if (!client) throw new Error("daemon is not connected");
    const patch = new TextDecoder().decode(await client.readArtifact(artifact));
    const files = reviewablePatchFiles(parseUnifiedDiff(patch));
    const roots = vscode.workspace.workspaceFolders ?? [];
    const root = roots.length === 1
      ? roots[0]
      : roots.length > 1
        ? await vscode.window.showQuickPick(
            roots.map((folder) => ({ label: folder.name, description: folder.uri.fsPath, folder })),
            { placeHolder: "Select the workspace containing this patch" },
          ).then((item) => item?.folder)
        : undefined;
    // Patch paths are repository-relative. With no unambiguous workspace root
    // (including a cancelled multi-root choice), fail closed before opening a diff.
    if (!root) return;
    const selected = files.length === 1
      ? files
      : await vscode.window.showQuickPick(files.map((file) => ({ label: file.path, file })), {
          canPickMany: true, placeHolder: "Select files to review",
        }).then((items) => items?.map((item) => item.file) ?? []);
    for (const file of selected) {
      const sourcePath = patchSourcePath(file);
      if (sourcePath !== undefined) {
        const sourceUri = vscode.Uri.joinPath(root.uri, sourcePath);
        if (sourceUri.scheme !== root.uri.scheme
          || sourceUri.authority !== root.uri.authority
          || !isWorkspaceUriPath(root.uri.path, sourceUri.path)) {
          throw new Error(`patch source path escapes the selected workspace: ${sourcePath}`);
        }
      }
      const before = await readPatchSource(file, async (path) => {
        const sourceUri = vscode.Uri.joinPath(root.uri, path);
        return vscode.workspace.fs.readFile(sourceUri);
      });
      const applied = applyUnifiedPatch(file, before);
      await showDiff(`${title}: ${file.path}`, `${file.path}-before`, `${file.path}-after`, applied.before, applied.after);
    }
  }

  // --- webview view ---------------------------------------------------------

  function post(message: TranscriptMessage): void {
    void view?.webview.postMessage(message);
  }

  const viewProvider: vscode.WebviewViewProvider = {
    resolveWebviewView(webviewView) {
      view = webviewView;
      const distUri = vscode.Uri.joinPath(context.extensionUri, "dist");
      webviewView.webview.options = { enableScripts: true, localResourceRoots: [distUri] };
      const nonce = makeNonce();
      webviewView.webview.html = renderPanelHtml({
        nonce,
        cspSource: webviewView.webview.cspSource,
        scriptUri: webviewView.webview.asWebviewUri(vscode.Uri.joinPath(distUri, "webview.js")).toString(),
      });
      webviewView.webview.onDidReceiveMessage((value: unknown) => {
        // The webview is CSP-sandboxed and only our own script posts here, but
        // validate the forwarded fields anyway: a malformed approvalId would be
        // serialized into a `Uuid` field and the daemon's deserialize failure
        // would drop the whole connection.
        if (typeof value !== "object" || value === null || !("kind" in value) || typeof value.kind !== "string") return;
        const raw = value as Partial<WebviewCommandMessage> & Record<string, unknown>;
        switch (raw.kind) {
          case "approve":
            if (typeof raw.approvalId === "string" && isUuid(raw.approvalId)) {
              client?.resolveApproval(raw.approvalId, "Approve");
            }
            break;
          case "reject":
            if (typeof raw.approvalId === "string" && isUuid(raw.approvalId)) {
              client?.resolveApproval(raw.approvalId, "Reject");
            }
            break;
          case "startRun":
            if (typeof raw.objective === "string" && raw.objective.trim().length > 0) {
              client?.startRun(raw.objective, "Build", workspaceRepo());
            }
            break;
          case "remoteUiRuntime":
            if (isUiRuntimeMessage(raw.message)) {
              rememberRemoteUiRuntime(raw.message);
              client?.sendRemoteUi(asProtocolWire(runtimeToWire(raw.message)));
            }
            break;
          case "remoteUiWire":
            if (isMediatedRuntimeWire(raw.message)) client?.sendRemoteUi(asProtocolWire(raw.message));
            break;
          case "remoteUiRecovery": {
            const action = raw.action;
            const documentId = raw.documentId;
            const extensionId = raw.extensionId;
            const message = raw.message;
            if ((action !== "disable" && action !== "report")
              || typeof documentId !== "string" || documentId.length === 0 || documentId.length > 256
              || typeof message !== "string" || message.length === 0 || message.length > 8_192
              || (extensionId !== undefined && (typeof extensionId !== "string" || extensionId.length === 0 || extensionId.length > 256))) break;
            output.appendLine(`[Remote UI ${action}] document=${documentId}${typeof extensionId === "string" ? ` plugin=${extensionId}` : ""}\n${message}`);
            if (action === "report") {
              output.show(true);
              void vscode.window.showInformationMessage("Remote UI diagnostics were written to the Codypendent output channel.");
            } else if (typeof extensionId === "string") {
              void vscode.window.showWarningMessage(
                `Disable Remote UI from ${extensionId}?`,
                { modal: true, detail: "This revokes the plugin and removes every surface it owns. You can reinstall and approve it later." },
                "Disable Surface",
              ).then((choice) => {
                if (choice === "Disable Surface") client?.revokeUiPlugin(extensionId);
              });
            }
            break;
          }
          case "remoteUiReady":
            if (isUiRuntimeMessage({ type: "capabilities", capabilities: raw.capabilities }) && Array.isArray(raw.documents) && raw.documents.length <= 1_000) {
              const capabilities = raw.capabilities as UiCapabilities;
              latestWebviewCapabilities = capabilities;
              client?.sendRemoteUi(asProtocolWire(runtimeToWire({ type: "capabilities", capabilities })));
              for (const document of raw.documents) {
                if (typeof document.documentId !== "string" || !Number.isSafeInteger(document.revision)) continue;
                pendingRemoteUiResyncs.set(document.documentId, document.revision);
                client?.sendRemoteUi(asProtocolWire(runtimeToWire({ type: "resync", documentId: document.documentId, knownRevision: document.revision })));
              }
            }
            break;
          case "sessionLibrarySearch": {
            const query = typeof raw.query === "string" ? raw.query.trim() : "";
            currentLibraryQuery = query;
            if (!client) {
              post({ kind: "sessionLibrary", status: "ready", items: [] });
              break;
            }
            client.searchSessions({ query, filters: {}, limit: 20 })
              .then((page) => {
                const items = page.items.map((item) => ({
                  stableIdentity: item.stable_identity,
                  sessionId: item.session.session_id,
                  title: item.session.title,
                  state: item.session.state,
                  updatedAt: item.session.updated_at,
                  pinned: Boolean(item.session.pinned),
                  archived: item.session.archived_at !== undefined && item.session.archived_at !== null,
                  source: typeof item.source === "object" && item.source !== null && "type" in item.source ? (item.source as { type: string }).type : "Session",
                  ...(item.excerpt ? { excerpt: item.excerpt } : {}),
                }));
                post({ kind: "sessionLibrary", status: "ready", items, nextCursor: page.next_cursor ?? undefined });
              })
              .catch((err) => {
                post({ kind: "sessionLibrary", status: "error", items: [], error: err instanceof Error ? err.message : String(err) });
              });
            break;
          }
          case "sessionLibraryLoadMore": {
            if (typeof raw.cursor !== "string" || !client) break;
            const query = typeof raw.query === "string" ? raw.query : currentLibraryQuery;
            client.searchSessions({ query, filters: {}, limit: 20, cursor: raw.cursor })
              .then((page) => {
                const items = page.items.map((item) => ({
                  stableIdentity: item.stable_identity,
                  sessionId: item.session.session_id,
                  title: item.session.title,
                  state: item.session.state,
                  updatedAt: item.session.updated_at,
                  pinned: Boolean(item.session.pinned),
                  archived: item.session.archived_at !== undefined && item.session.archived_at !== null,
                  source: typeof item.source === "object" && item.source !== null && "type" in item.source ? (item.source as { type: string }).type : "Session",
                  ...(item.excerpt ? { excerpt: item.excerpt } : {}),
                }));
                post({ kind: "sessionLibrary", status: "ready", items, nextCursor: page.next_cursor ?? undefined, append: true });
              })
              .catch((err) => {
                post({ kind: "sessionLibrary", status: "error", items: [], error: err instanceof Error ? err.message : String(err) });
              });
            break;
          }
          case "sessionLibraryOpen": {
            if (typeof raw.sessionId === "string" && isUuid(raw.sessionId)) {
              connect(raw.sessionId);
              void vscode.commands.executeCommand("codypendent.sessionView.focus");
            }
            break;
          }
          case "sessionLibraryMutate": {
            if (typeof raw.sessionId === "string" && isUuid(raw.sessionId) && typeof raw.action === "object" && raw.action !== null && "type" in raw.action) {
              client?.mutateSessionLifecycle(
                raw.sessionId,
                raw.action as unknown as GeneratedSessionLifecycleAction,
              );
            }
            break;
          }
        }
      });
      // Clear the module reference when the user closes/disposes the view —
      // a postMessage to a disposed webview rejects, and `post` guards only
      // on `undefined`.
      webviewView.onDidDispose(() => {
        if (view === webviewView) {
          view = undefined;
        }
      });
      post({ kind: "status", status: client?.connectionStatus ?? "closed" });
      post({
        kind: "remoteUiConfigure",
        showTerminalFallback: vscode.workspace.getConfiguration("codypendent").get<boolean>("remoteUi.terminalFallbackPreview", false),
      });
    },
  };
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider("codypendent.sessionView", viewProvider, {
      // Keep the webview's DOM (transcript + pending approval cards) alive when
      // the view is hidden, so switching the activity bar away and back does not
      // blank the panel or drop an unresolved approval card.
      webviewOptions: { retainContextWhenHidden: true },
    }),
  );

  // --- daemon client --------------------------------------------------------

  function connect(sessionId: Uuid, handoff?: SessionHandoff): void {
    client?.stop();
    let socketPath: string;
    const configured = vscode.workspace
      .getConfiguration("codypendent")
      .get<string>("socketPath")
      ?.trim();
    try {
      socketPath = handoff?.socket ?? attachedSocketPath
        ?? (configured && configured.length > 0 ? configured : resolveRuntimePaths().socketPath);
    } catch (err) {
      void vscode.window.showErrorMessage(
        `Codypendent: cannot resolve daemon socket: ${err instanceof Error ? err.message : String(err)}`,
      );
      return;
    }

    output.appendLine(`Connecting to ${socketPath} for session ${sessionId}`);
    pendingRemoteUiResyncs.clear();
    post({ kind: "clear" });

    const nextClient = new DaemonClient({ socketPath, sessionId });
    client = nextClient;
    attachedSessionId = sessionId;
    attachedSocketPath = socketPath;
    attachedRepository = handoff?.repository ?? attachedRepository;
    // A new attach is a new projection: forget which ids were already
    // announced so the fresh catch-up can speak for itself.
    notifier.reset();
    inboxTreeProvider.setClient(nextClient);

    nextClient.on("status", (status: ConnectionStatus) => {
      post({ kind: "status", status });
      output.appendLine(`status: ${status}`);
      if (status === "attached") {
        flushRemoteUiRuntime(nextClient);
        void refreshInbox();
      }
    });
    nextClient.on("serverHello", (hello) => {
      output.appendLine(`server hello: daemon ${hello.daemon_version}, protocol ${hello.selected_protocol.major}.${hello.selected_protocol.minor}`);
    });
    nextClient.on("event", (event) => {
      handleEvent(event, post, true, reviewPatch);
      // Only events that change what the inbox shows may refresh it. This
      // fired on EVERY event, and a streaming reply is tens of
      // `ModelStreamDelta`s a second — each one a full `ListInbox` round trip
      // to render a list that had not changed. The inbox holds blocking work,
      // so the two predicates that decide whether blocking work was raised or
      // resolved are exactly the right question, and reusing them keeps this
      // in step with the notifier rather than beside it.
      if (blockingWorkOf(event, sessionId) !== null || resolvedWorkKeyOf(event) !== null) {
        void refreshInbox();
      }
    });
    // The notification path, wired once per connection: live events and both
    // shapes of catch-up. Nothing else in this file raises a notification for
    // blocking work.
    subscribeBlockingWork(nextClient, sessionId, notifier, (approval) =>
      `${describeAction(approval.action)} (risk: ${describeRisk(approval.risk)})`,
    );
    nextClient.on("remoteUi", (message) => {
      const projection = wireToHost(asRemoteUiWire(message));
      for (const hostMessage of projection.messages) {
        const documentId = hostMessage.type === "snapshot"
          ? hostMessage.document.documentId
          : hostMessage.type === "patch"
            ? hostMessage.batch.documentId
            : hostMessage.type === "dispose" || hostMessage.type === "error"
              ? hostMessage.documentId
              : undefined;
        const rawPlacement = documentId === undefined ? undefined : projection.placements.get(documentId);
        const placement = rawPlacement === undefined
          ? undefined
          : {
              point: rawPlacement.point as import("@codypendent/ui").ContributionPoint,
              ...(rawPlacement.extensionId === undefined ? {} : { extensionId: rawPlacement.extensionId }),
              ...(rawPlacement.ownerScope === undefined ? {} : { ownerScope: rawPlacement.ownerScope }),
              ...(rawPlacement.publisher === undefined ? {} : { publisher: rawPlacement.publisher }),
              ...(rawPlacement.trust === undefined ? {} : { trust: rawPlacement.trust }),
              ...(rawPlacement.slot === undefined ? {} : { slot: rawPlacement.slot }),
              ...(rawPlacement.priority === undefined ? {} : { priority: rawPlacement.priority }),
            };
        post({ kind: "remoteUi", message: hostMessage, ...(placement === undefined ? {} : { placement }) });
        if (documentId !== undefined && hostMessage.type === "snapshot") pendingRemoteUiResyncs.delete(documentId);
      }
      for (const [documentId, rawPlacement] of projection.placements) {
        post({
          kind: "remoteUiPlacement",
          documentId,
          placement: {
            point: rawPlacement.point as import("@codypendent/ui").ContributionPoint,
            ...(rawPlacement.extensionId === undefined ? {} : { extensionId: rawPlacement.extensionId }),
            ...(rawPlacement.ownerScope === undefined ? {} : { ownerScope: rawPlacement.ownerScope }),
            ...(rawPlacement.publisher === undefined ? {} : { publisher: rawPlacement.publisher }),
            ...(rawPlacement.trust === undefined ? {} : { trust: rawPlacement.trust }),
            ...(rawPlacement.slot === undefined ? {} : { slot: rawPlacement.slot }),
            ...(rawPlacement.priority === undefined ? {} : { priority: rawPlacement.priority }),
          },
        });
      }
      if (projection.contributionReplacement !== undefined) {
        post({
          kind: "remoteUiContributions",
          owner: projection.contributionReplacement.owner,
          registrations: projection.contributionReplacement.registrations.map((registration) => ({
            documentId: registration.documentId,
            placement: {
              point: registration.point as import("@codypendent/ui").ContributionPoint,
              extensionId: registration.extensionId,
              ...(registration.ownerScope === undefined ? {} : { ownerScope: registration.ownerScope }),
              ...(registration.publisher === undefined ? {} : { publisher: registration.publisher }),
              ...(registration.trust === undefined ? {} : { trust: registration.trust }),
              ...(registration.slot === undefined ? {} : { slot: registration.slot }),
              ...(registration.priority === undefined ? {} : { priority: registration.priority }),
            },
          })),
        });
      }
      for (const mediated of projection.mediated) post({ kind: "remoteUiWire", message: mediated });
      if (projection.theme !== undefined) post({ kind: "remoteUiTheme", theme: projection.theme });
    });
    // Render the events replayed on attach/reconnect so the transcript is not
    // blank after opening or reloading an existing session. Replay is
    // non-interactive: historical approvals/diffs are shown as trace, never
    // re-prompted (the live stream drives any still-pending approval).
    nextClient.on("catchup", (catchup) => {
      if (catchup.type === "Events") {
        for (const event of catchup.events) {
          handleEvent(event, post, false);
        }
      } else if (catchup.type === "Snapshot") {
        // A long session catches up as a compacted Snapshot (no event list).
        // Seed the panel from the projection so it is not blank: the session
        // title plus a stub per active run.
        const projection = catchup.projection;
        post({ kind: "event", sequence: projection.last_sequence, label: "session", detail: projection.title });
        for (const runId of projection.active_runs ?? []) {
          post({ kind: "runState", runId, state: "Running" });
        }
        for (const approval of projection.pending_approvals ?? []) {
          post({
            kind: "approval",
            approvalId: approval.approval_id,
            summary: describeAction(approval.action),
            risk: describeRisk(approval.risk),
          });
        }
      }
    });
    nextClient.on("commandRejected", (error) => {
      void vscode.window.showWarningMessage(`Codypendent: command rejected (${error.code}): ${error.message}`);
    });
    nextClient.on("protocolError", (error) => {
      output.appendLine(`protocol error (${error.code}): ${error.message}`);
    });
    nextClient.on("error", (error) => {
      output.appendLine(`error: ${error.message}`);
    });
    // FP-5: the offline queue never silently drops an approval decision, but
    // when it is saturated with other pending approvals it must refuse a new
    // one rather than grow unbounded — that refusal is loud, not just logged,
    // since it means the user's decision did not go through and needs redoing.
    nextClient.on("approvalDropped", ({ approvalId }) => {
      void vscode.window.showWarningMessage(
        `Codypendent: too many approvals are queued while offline; the decision for approval ${approvalId} was NOT saved — resolve it again once reconnected.`,
      );
    });

    nextClient.start();
    context.subscriptions.push({ dispose: () => nextClient.stop() });
  }

  // A URI can originate in a browser, so it never silently attaches to a new
  // daemon or changes workspace. The approved continuation is local, expires,
  // and is consumed only by the extension host for the requested checkout.
  const pendingHandoffKey = "codypendent.pendingHandoff.v1";
  async function acceptHandoff(raw: SessionHandoff, confirmed = false): Promise<void> {
    const handoff = await validateHandoff(raw);
    if (!confirmed && (attachedSessionId !== handoff.session || attachedSocketPath !== handoff.socket
        || attachedRepository !== handoff.repository)) {
      const choice = await vscode.window.showInformationMessage(
        "Open this Codypendent session?",
        { modal: true, detail: `Repository: ${handoff.repository}${handoff.checkout !== handoff.repository ? `\nRun checkout: ${handoff.checkout}` : ""}\nSession: ${handoff.session}\nLocal daemon: ${handoff.socket}` },
        "Open Session",
      );
      if (choice !== "Open Session") return;
    }
    const roots = (vscode.workspace.workspaceFolders ?? [])
      .filter((folder) => folder.uri.scheme === "file").map((folder) => folder.uri.fsPath);
    if (!await workspaceMatches(handoff.checkout ?? handoff.repository, roots)) {
      await context.globalState.update(pendingHandoffKey, { handoff, expires: Date.now() + 120_000 });
      await vscode.commands.executeCommand("vscode.openFolder", vscode.Uri.file(handoff.checkout ?? handoff.repository), { forceNewWindow: true });
      return;
    }
    if (!vscode.workspace.isTrusted) {
      const listener = vscode.workspace.onDidGrantWorkspaceTrust(() => {
        listener.dispose();
        void acceptHandoff(handoff, true).catch(handoffError);
      });
      context.subscriptions.push(listener);
      const choice = await vscode.window.showWarningMessage(
        "Trust this workspace before attaching Codypendent.", "Manage Workspace Trust",
      );
      if (choice === "Manage Workspace Trust") {
        await vscode.commands.executeCommand("workbench.trust.manage");
      }
      return;
    }
    const validated = await validateHandoff(handoff);
    await vscode.commands.executeCommand("codypendent.sessionView.focus");
    if (attachedSessionId !== validated.session || attachedSocketPath !== validated.socket
        || attachedRepository !== validated.repository) {
      connect(validated.session, validated);
    }
    if (validated.file) {
      const uri = vscode.Uri.joinPath(vscode.Uri.file(validated.checkout ?? validated.repository), validated.file);
      const document = await vscode.workspace.openTextDocument(uri);
      const row = Math.min((validated.line ?? 1) - 1, document.lineCount - 1);
      const editor = await vscode.window.showTextDocument(document, {
        preview: false, selection: new vscode.Range(row, 0, row, 0),
      });
      editor.revealRange(new vscode.Range(row, 0, row, 0), vscode.TextEditorRevealType.InCenterIfOutsideViewport);
    }
  }
  const handoffError = (error: unknown) => {
    void vscode.window.showErrorMessage(`Codypendent: could not open session: ${error instanceof Error ? error.message : String(error)}`);
  };
  context.subscriptions.push(vscode.window.registerUriHandler({
    async handleUri(uri) {
      try {
        if (uri.authority !== "codypendent.codypendent" || uri.path !== "/attach" || uri.fragment) {
          throw new Error("Unsupported session link");
        }
        await acceptHandoff(parseHandoff(uri.query));
      } catch (error) { handoffError(error); }
    },
  }));
  const pending = context.globalState.get<{ handoff: SessionHandoff; expires: number }>(pendingHandoffKey);
  if (pending && pending.expires > Date.now()) {
    void workspaceMatches(pending.handoff.checkout ?? pending.handoff.repository,
      (vscode.workspace.workspaceFolders ?? []).map((folder) => folder.uri.fsPath)).then(async (matches) => {
      if (!matches) return;
      await context.globalState.update(pendingHandoffKey, undefined);
      await acceptHandoff(pending.handoff, true);
    }).catch(handoffError);
  } else if (pending) {
    void context.globalState.update(pendingHandoffKey, undefined);
  }

  async function ensureSession(promptTitle: string): Promise<Uuid | undefined> {
    const configured = vscode.workspace.getConfiguration("codypendent").get<string>("sessionId")?.trim();
    if (configured && configured.length > 0) {
      return configured;
    }
    // `codypendent open --in vscode` launches the editor with the session in the
    // environment; honor it (after validation) so the handoff auto-attaches
    // instead of prompting for a UUID the user does not have.
    const fromEnv = process.env.CODYPENDENT_SESSION?.trim();
    if (fromEnv && isUuid(fromEnv)) {
      return fromEnv;
    }
    const entered = await vscode.window.showInputBox({
      title: promptTitle,
      prompt: "Session UUID to attach to",
      placeHolder: "00000000-0000-0000-0000-000000000000",
      validateInput: (value) => (isUuid(value.trim()) ? undefined : "Enter a valid UUID"),
    });
    return entered?.trim();
  }

  function handleDeepLink(deepLink: InboxDeepLink): void {
    if (deepLink.type === "Session") {
      connect(deepLink.session_id);
      void vscode.commands.executeCommand("codypendent.sessionView.focus");
    } else if (deepLink.type === "Run") {
      connect(deepLink.session_id);
      void vscode.commands.executeCommand("codypendent.sessionView.focus");
    } else if (deepLink.type === "Approval") {
      void vscode.commands.executeCommand("codypendent.approve");
    }
  }

  // --- commands -------------------------------------------------------------

  context.subscriptions.push(
    vscode.commands.registerCommand("codypendent.showInboxQuickPick", () => {
      return showInboxQuickPick(client, inboxTreeProvider, handleDeepLink);
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("codypendent.refreshInbox", () => {
      return refreshInbox();
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("codypendent.openInboxEntry", (entry?: InboxEntry) => {
      if (entry?.deep_link) {
        handleDeepLink(entry.deep_link);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("codypendent.acknowledgeInboxEntry", async (item?: InboxItemTreeItem) => {
      if (!client) {
        void vscode.window.showWarningMessage("Codypendent: not connected to daemon.");
        return;
      }
      const entryId = item?.entry?.id;
      if (!entryId) return;
      try {
        await client.mutateInbox({ type: "Acknowledge", entry_id: entryId });
        void vscode.window.showInformationMessage("Acknowledged notification.");
        await refreshInbox();
      } catch (err) {
        void vscode.window.showErrorMessage(`Failed to acknowledge: ${err instanceof Error ? err.message : String(err)}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("codypendent.dismissInboxEntry", async (item?: InboxItemTreeItem) => {
      if (!client) {
        void vscode.window.showWarningMessage("Codypendent: not connected to daemon.");
        return;
      }
      const entryId = item?.entry?.id;
      if (!entryId) return;
      try {
        await client.mutateInbox({ type: "Dismiss", entry_id: entryId });
        void vscode.window.showInformationMessage("Dismissed notification.");
        await refreshInbox();
      } catch (err) {
        void vscode.window.showErrorMessage(`Failed to dismiss: ${err instanceof Error ? err.message : String(err)}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("codypendent.openSession", async () => {
      const sessionId = await ensureSession("Codypendent: Open Session");
      if (!sessionId) {
        return;
      }
      connect(sessionId);
      await vscode.commands.executeCommand("codypendent.sessionView.focus");
    }),
  );

  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (!event.affectsConfiguration("codypendent.remoteUi.terminalFallbackPreview")) return;
      post({
        kind: "remoteUiConfigure",
        showTerminalFallback: vscode.workspace.getConfiguration("codypendent").get<boolean>("remoteUi.terminalFallbackPreview", false),
      });
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("codypendent.approve", async () => {
      if (!client) {
        void vscode.window.showWarningMessage("Codypendent: not attached to a session.");
        return;
      }
      const approvalId = await vscode.window.showInputBox({
        title: "Codypendent: Resolve Approval",
        prompt: "Approval UUID to resolve",
        validateInput: (value) => (isUuid(value.trim()) ? undefined : "Enter a valid UUID"),
      });
      if (!approvalId) {
        return;
      }
      const decision = await vscode.window.showQuickPick(["Approve", "Reject"], {
        title: "Decision",
      });
      if (decision !== "Approve" && decision !== "Reject") {
        return;
      }
      client.resolveApproval(approvalId.trim(), decision);
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("codypendent.startRun", async () => {
      if (!client) {
        void vscode.window.showWarningMessage("Codypendent: not attached to a session.");
        return;
      }
      const objective = await vscode.window.showInputBox({
        title: "Codypendent: Start Run",
        prompt: "Objective for the run",
      });
      if (!objective) {
        return;
      }
      client.startRun(objective, "Build", workspaceRepo());
    }),
  );

  // --- IDE context push (debounced >= 300 ms) -------------------------------

  let debounceTimer: ReturnType<typeof setTimeout> | undefined;
  let diagnosticsRevision = 0;

  function scheduleContextPush(): void {
    if (debounceTimer) {
      clearTimeout(debounceTimer);
    }
    debounceTimer = setTimeout(() => {
      debounceTimer = undefined;
      client?.sendIdeContext(buildIdeContext(diagnosticsRevision));
    }, IDE_CONTEXT_DEBOUNCE_MS);
  }

  context.subscriptions.push(
    vscode.window.onDidChangeActiveTextEditor(() => scheduleContextPush()),
    vscode.window.onDidChangeTextEditorSelection(() => scheduleContextPush()),
    vscode.workspace.onDidChangeTextDocument(() => scheduleContextPush()),
    vscode.workspace.onDidOpenTextDocument(() => scheduleContextPush()),
    vscode.workspace.onDidCloseTextDocument(() => scheduleContextPush()),
    vscode.languages.onDidChangeDiagnostics(() => {
      diagnosticsRevision += 1;
      scheduleContextPush();
    }),
    { dispose: () => debounceTimer && clearTimeout(debounceTimer) },
  );

  // Register editor-native actions (Explain, Refactor, Fix Diagnostic, Generate Tests, Review)
  registerEditorActions(context, () => client, () => diagnosticsRevision, workspaceRepo);

  // Auto-attach on startup when a session id is already configured.
  const configuredSession = vscode.workspace
    .getConfiguration("codypendent")
    .get<string>("sessionId")
    ?.trim();
  if (!pending && vscode.workspace.isTrusted && configuredSession && isUuid(configuredSession)) {
    connect(configuredSession);
  }
}

function rememberRemoteUiRuntime(message: UiRuntimeMessage): void {
  if (message.type === "capabilities") latestWebviewCapabilities = message.capabilities;
  if (message.type === "resync") pendingRemoteUiResyncs.set(message.documentId, message.knownRevision);
}

function flushRemoteUiRuntime(target: DaemonClient): void {
  if (latestWebviewCapabilities !== undefined) {
    target.sendRemoteUi(asProtocolWire(runtimeToWire({ type: "capabilities", capabilities: latestWebviewCapabilities })));
  }
  for (const [documentId, knownRevision] of pendingRemoteUiResyncs) {
    target.sendRemoteUi(asProtocolWire(runtimeToWire({ type: "resync", documentId, ...(knownRevision === undefined ? {} : { knownRevision }) })));
  }
}

export function deactivate(): void {
  client?.stop();
  client = undefined;
  attachedSessionId = undefined;
  attachedSocketPath = undefined;
  attachedRepository = undefined;
  view = undefined;
}

// ---------------------------------------------------------------------------
// Event -> transcript rendering
// ---------------------------------------------------------------------------

function handleEvent(
  event: SessionEvent,
  post: (message: TranscriptMessage) => void,
  interactive: boolean,
  reviewPatch?: (artifact: ArtifactRef, title: string) => Promise<void>,
): void {
  const body = event.body;
  switch (body.type) {
    case "RunStateChanged":
      post({ kind: "runState", runId: body.run_id, state: body.state.type });
      post({ kind: "event", sequence: event.sequence, label: "run", detail: body.state.type });
      break;
    case "ModelStreamDelta":
      post({ kind: "event", sequence: event.sequence, label: "model", detail: body.text });
      break;
    // A single approval surfaces as BOTH `ToolProposed` (from the runtime) and
    // `ApprovalRequested` (from the broker). Prompt on exactly one —
    // `ApprovalRequested`, which also carries the risk — so the user sees one
    // dialog and we send one `ResolveApproval`; `ToolProposed` is trace-only.
    case "ToolProposed":
      post({ kind: "event", sequence: event.sequence, label: "tool", detail: describeAction(body.action) });
      break;
    case "ToolDenied":
      post({
        kind: "event",
        sequence: event.sequence,
        label: "tool denied",
        detail: `${describeAction(body.action)}${body.reasons?.length ? ` — ${body.reasons.join("; ")}` : ""}`,
      });
      break;
    // The approval CARD is drawn from every occurrence, live or replayed. The
    // notification is not raised here: `BlockingWorkNotifier` owns that, so a
    // replayed-but-already-resolved approval cannot raise a toast (see the
    // `catchup` handler).
    case "ApprovalRequested":
      post({
        kind: "approval",
        approvalId: body.approval_id,
        summary: describeAction(body.action),
        risk: describeRisk(body.risk),
      });
      break;
    case "ApprovalResolved":
      post({ kind: "approvalResolved", approvalId: body.approval_id, decision: body.decision.type });
      break;
    case "PatchProposed":
      post({
        kind: "event",
        sequence: event.sequence,
        label: "patch",
        detail: `changeset ${body.changeset_id} (${body.artifact.byte_length} bytes)`,
      });
      // The patch bytes travel as an artifact (fetched out-of-band); show the
      // available metadata as a diff placeholder so the change set is visible.
      // Only for a live event — replaying history must not reopen old diffs.
      if (interactive) {
        void reviewPatch?.(body.artifact, `Codypendent change set ${body.changeset_id.slice(0, 8)}`)
          .catch((error) => void vscode.window.showErrorMessage(`Could not review patch: ${String(error)}`));
      }
      break;
    case "RunStarted":
      post({ kind: "event", sequence: event.sequence, label: "run started", detail: body.objective });
      break;
    case "RunCompleted":
      post({ kind: "event", sequence: event.sequence, label: "run completed", detail: body.disposition.type });
      break;
    case "ToolStarted":
      post({ kind: "event", sequence: event.sequence, label: "tool", detail: body.label || body.tool });
      break;
    case "ToolCompleted":
      post({ kind: "event", sequence: event.sequence, label: "tool done", detail: body.tool.startsWith("acp:")
        ? body.outcome.type : `${body.tool}: ${body.outcome.type}` });
      break;
    case "ToolUpdated":
      if (body.label) post({ kind: "event", sequence: event.sequence, label: "tool action", detail: body.label });
      break;
    case "BudgetWarning":
      post({
        kind: "event",
        sequence: event.sequence,
        label: "budget",
        detail: `${body.dimension.type} ${body.used}/${body.limit}`,
      });
      break;
    case "NoteAppended":
      post({ kind: "event", sequence: event.sequence, label: "note", detail: body.text });
      break;
    case "SessionCreated":
      post({ kind: "event", sequence: event.sequence, label: "session", detail: body.title });
      break;
    default:
      post({ kind: "event", sequence: event.sequence, label: body.type, detail: "" });
      break;
  }
}

function describeAction(action: WireProposedAction): string {
  switch (action.type) {
    case "ReadFiles":
      return `read ${Array.isArray(action.paths) ? action.paths.length : 0} file(s)`;
    case "WritePatch":
      return `write patch ${String(action.patch)}`;
    case "ExecuteCommand": {
      // The environment and cwd MUST be on the card: the protocol carries them
      // precisely so an approver cannot be tricked by a benign-looking command
      // with a smuggled LD_PRELOAD/RUSTC_WRAPPER/shadowed-PATH binding or a
      // surprising working directory (mirrors the TUI's approval card).
      let text =
        `run ${String(action.program)} ${Array.isArray(action.args) ? action.args.join(" ") : ""}`.trim();
      if (typeof action.cwd === "string" && action.cwd.length > 0) {
        text += ` [cwd: ${action.cwd}]`;
      }
      const environment = Array.isArray(action.environment) ? action.environment : [];
      if (environment.length > 0) {
        const bindings = environment
          .filter((pair): pair is [string, string] => Array.isArray(pair) && pair.length === 2)
          .map(([name, value]) => `${name}=${value}`)
          .join(", ");
        text += ` [env: ${bindings}]`;
      } else {
        text += " [env: empty]";
      }
      return text;
    }
    case "NetworkRequest":
      return `network request to ${String(action.destination)}`;
    case "GitCommit":
      return `git commit in ${String(action.repository)}`;
    case "GitPush":
      return `git push ${String(action.branch)} -> ${String(action.remote)}`;
    case "GitHubMutation":
      return String(action.summary);
    case "McpToolCall":
      return `${String(action.summary)} args: ${String(action.args)}`;
    case "AcpToolCall":
      return `${String(action.agent)} · ${String(action.title)}: ${String(action.details)}`;
    case "PublishDocument": {
      const files = Array.isArray(action.changed_files)
        ? action.changed_files.map(String).join(", ")
        : "";
      return `publish document to ${String(action.target)}; files: ${files || "none"}; git: ${String(action.git_action)}`;
    }
    case "RecordMemory":
      // Never actually reaches the client (always-Allow, never on the wire —
      // see the `ProposedAction` doc comment above); handled for completeness.
      return "record a memory";
    default:
      return `action ${action.type}`;
  }
}

function describeRisk(risk: WireRisk): string {
  const reasons = Array.isArray(risk.reasons) && risk.reasons.length > 0 ? ` (${risk.reasons.join("; ")})` : "";
  return `${risk.level.type}${reasons}`;
}

// ---------------------------------------------------------------------------
// IDE context snapshot
// ---------------------------------------------------------------------------

function buildIdeContext(diagnosticsRevision: number): IdeContextUpdate {
  const editor = vscode.window.activeTextEditor;
  const activeFile = editor?.document.uri.scheme === "file" ? editor.document.uri.fsPath : undefined;

  let selection: EditorSelection | undefined;
  if (editor && activeFile) {
    const sel = editor.selection;
    selection = {
      path: activeFile,
      range: {
        start: { line: sel.start.line, character: sel.start.character },
        end: { line: sel.end.line, character: sel.end.character },
      },
    };
  }

  const openFiles: string[] = [];
  const dirtyBuffers: DirtyBufferDigest[] = [];
  for (const doc of vscode.workspace.textDocuments) {
    if (doc.uri.scheme !== "file") {
      continue;
    }
    openFiles.push(doc.uri.fsPath);
    if (doc.isDirty) {
      const text = doc.getText();
      dirtyBuffers.push({
        path: doc.uri.fsPath,
        sha256: createHash("sha256").update(text, "utf8").digest("hex"),
        byte_length: Buffer.byteLength(text, "utf8"),
      });
    }
  }

  const update: IdeContextUpdate = { diagnostics_revision: diagnosticsRevision };
  if (activeFile !== undefined) {
    update.active_file = activeFile;
  }
  if (selection !== undefined) {
    update.selection = selection;
  }
  if (openFiles.length > 0) {
    update.open_files = openFiles;
  }
  if (dirtyBuffers.length > 0) {
    update.dirty_buffers = dirtyBuffers;
  }
  return update;
}

function isUuid(value: string): boolean {
  return /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/.test(value);
}

/**
 * The first workspace folder's filesystem path, attributed to a started run so a
 * shared daemon stores its memories under the run's repository — not the
 * daemon's own startup directory (issue #6 item 1). Undefined outside a folder.
 */
function workspaceRepo(): string | undefined {
  return attachedRepository ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
}
