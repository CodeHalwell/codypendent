/**
 * VS Code / Cursor extension entry point for Codypendent (Phase 3 STEP 3.5).
 *
 * This is the ONLY module that imports `vscode`; every protocol/transport concern
 * lives in the pure, unit-tested modules under `src/protocol/` and `src/client.ts`.
 *
 * Responsibilities:
 *   - resolve the daemon socket exactly as the daemon does and open a
 *     {@link DaemonClient} (attach as Contributor, reconnect + attach-resume);
 *   - render the session transcript and run state in a side-panel webview from
 *     the daemon's events / catch-up projections;
 *   - surface approvals as `showInformationMessage(Approve/Reject)` -> the
 *     `ResolveApproval` command;
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
import { resolveRuntimePaths } from "./protocol/discovery.js";
import {
  makeNonce,
  renderPanelHtml,
  type TranscriptMessage,
  type WebviewCommandMessage,
} from "./webview/panel.js";
import type {
  DirtyBufferDigest,
  EditorSelection,
  IdeContextUpdate,
  ProposedAction,
  Risk,
  SessionEvent,
  Uuid,
} from "./protocol/types.js";

const IDE_CONTEXT_DEBOUNCE_MS = 300;
const DIFF_SCHEME = "codypendent-diff";

let client: DaemonClient | undefined;
let view: vscode.WebviewView | undefined;

export function activate(context: vscode.ExtensionContext): void {
  const output = vscode.window.createOutputChannel("Codypendent");
  context.subscriptions.push(output);

  // Virtual-document provider backing `vscode.diff` for proposed change sets.
  const diffContents = new Map<string, string>();
  const diffProvider: vscode.TextDocumentContentProvider = {
    provideTextDocumentContent(uri) {
      return diffContents.get(uri.toString()) ?? "";
    },
  };
  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider(DIFF_SCHEME, diffProvider),
  );

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
    diffContents.set(leftUri.toString(), left);
    diffContents.set(rightUri.toString(), right);
    await vscode.commands.executeCommand("vscode.diff", leftUri, rightUri, title);
  }

  // --- webview view ---------------------------------------------------------

  function post(message: TranscriptMessage): void {
    void view?.webview.postMessage(message);
  }

  const viewProvider: vscode.WebviewViewProvider = {
    resolveWebviewView(webviewView) {
      view = webviewView;
      webviewView.webview.options = { enableScripts: true };
      const nonce = makeNonce();
      webviewView.webview.html = renderPanelHtml({
        nonce,
        cspSource: webviewView.webview.cspSource,
      });
      webviewView.webview.onDidReceiveMessage((raw: WebviewCommandMessage) => {
        switch (raw.kind) {
          case "approve":
            client?.resolveApproval(raw.approvalId, "Approve");
            break;
          case "reject":
            client?.resolveApproval(raw.approvalId, "Reject");
            break;
          case "startRun":
            client?.startRun(raw.objective);
            break;
        }
      });
      post({ kind: "status", status: client?.connectionStatus ?? "closed" });
    },
  };
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider("codypendent.sessionView", viewProvider),
  );

  // --- daemon client --------------------------------------------------------

  function connect(sessionId: Uuid): void {
    client?.stop();
    let socketPath: string;
    const configured = vscode.workspace
      .getConfiguration("codypendent")
      .get<string>("socketPath")
      ?.trim();
    try {
      socketPath = configured && configured.length > 0 ? configured : resolveRuntimePaths().socketPath;
    } catch (err) {
      void vscode.window.showErrorMessage(
        `Codypendent: cannot resolve daemon socket: ${err instanceof Error ? err.message : String(err)}`,
      );
      return;
    }

    output.appendLine(`Connecting to ${socketPath} for session ${sessionId}`);
    post({ kind: "clear" });

    const nextClient = new DaemonClient({ socketPath, sessionId });
    client = nextClient;

    nextClient.on("status", (status: ConnectionStatus) => {
      post({ kind: "status", status });
      output.appendLine(`status: ${status}`);
    });
    nextClient.on("serverHello", (hello) => {
      output.appendLine(`server hello: daemon ${hello.daemon_version}, protocol ${hello.selected_protocol.major}.${hello.selected_protocol.minor}`);
    });
    nextClient.on("event", (event) => {
      handleEvent(event, post, showDiff, true);
    });
    // Render the events replayed on attach/reconnect so the transcript is not
    // blank after opening or reloading an existing session. Replay is
    // non-interactive: historical approvals/diffs are shown as trace, never
    // re-prompted (the live stream drives any still-pending approval).
    nextClient.on("catchup", (catchup) => {
      if (catchup.type === "Events") {
        for (const event of catchup.events) {
          handleEvent(event, post, showDiff, false);
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

    nextClient.start();
    context.subscriptions.push({ dispose: () => nextClient.stop() });
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

  // --- commands -------------------------------------------------------------

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
      client.startRun(objective);
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

  // Auto-attach on startup when a session id is already configured.
  const configuredSession = vscode.workspace
    .getConfiguration("codypendent")
    .get<string>("sessionId")
    ?.trim();
  if (configuredSession && isUuid(configuredSession)) {
    connect(configuredSession);
  }
}

export function deactivate(): void {
  client?.stop();
  client = undefined;
  view = undefined;
}

// ---------------------------------------------------------------------------
// Event -> transcript rendering
// ---------------------------------------------------------------------------

function handleEvent(
  event: SessionEvent,
  post: (message: TranscriptMessage) => void,
  showDiff: (t: string, l: string, r: string, left: string, right: string) => Promise<void>,
  interactive: boolean,
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
    case "ApprovalRequested":
      post({
        kind: "approval",
        approvalId: body.approval_id,
        summary: describeAction(body.action),
        risk: describeRisk(body.risk),
      });
      if (interactive) {
        void promptApproval(body.approval_id, describeAction(body.action), describeRisk(body.risk));
      }
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
        void showDiff(
          `Codypendent change set ${body.changeset_id.slice(0, 8)}`,
          "HEAD",
          "proposed",
          "",
          `# proposed patch\n# artifact ${body.artifact.id}\n# sha256 ${body.artifact.sha256}\n# ${body.artifact.byte_length} bytes, media ${body.artifact.media_type}\n`,
        );
      }
      break;
    case "RunStarted":
      post({ kind: "event", sequence: event.sequence, label: "run started", detail: body.objective });
      break;
    case "RunCompleted":
      post({ kind: "event", sequence: event.sequence, label: "run completed", detail: body.disposition.type });
      break;
    case "ToolStarted":
      post({ kind: "event", sequence: event.sequence, label: "tool", detail: body.tool });
      break;
    case "ToolCompleted":
      post({ kind: "event", sequence: event.sequence, label: "tool done", detail: `${body.tool}: ${body.outcome.type}` });
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

async function promptApproval(approvalId: Uuid, summary: string, risk: string): Promise<void> {
  const choice = await vscode.window.showInformationMessage(
    `Approval required: ${summary} (risk: ${risk})`,
    { modal: false },
    "Approve",
    "Reject",
  );
  if (choice === "Approve") {
    client?.resolveApproval(approvalId, "Approve");
  } else if (choice === "Reject") {
    client?.resolveApproval(approvalId, "Reject");
  }
}

function describeAction(action: ProposedAction): string {
  switch (action.type) {
    case "ReadFiles":
      return `read ${Array.isArray(action.paths) ? action.paths.length : 0} file(s)`;
    case "WritePatch":
      return `write patch ${String(action.patch)}`;
    case "ExecuteCommand":
      return `run ${String(action.program)} ${Array.isArray(action.args) ? action.args.join(" ") : ""}`.trim();
    case "NetworkRequest":
      return `network request to ${String(action.destination)}`;
    case "GitCommit":
      return `git commit in ${String(action.repository)}`;
    case "GitPush":
      return `git push ${String(action.branch)} -> ${String(action.remote)}`;
    case "GitHubMutation":
      return String(action.summary);
    default:
      return `action ${action.type}`;
  }
}

function describeRisk(risk: Risk): string {
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
