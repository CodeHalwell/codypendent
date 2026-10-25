import { createHash } from "node:crypto";
import * as vscode from "vscode";

import type { DaemonClient } from "./client.js";
import type {
  Diagnostic,
  DiagnosticSeverity,
  DirtyBufferDigest,
  EditorActionContext,
  EditorNativeAction,
  EditorSelection,
  IdeContextUpdate,
} from "@codypendent/protocol";

/** Convert a VS Code DiagnosticSeverity into the protocol DiagnosticSeverity. */
export function toProtocolDiagnosticSeverity(severity: vscode.DiagnosticSeverity): DiagnosticSeverity {
  switch (severity) {
    case vscode.DiagnosticSeverity.Error:
      return { type: "Error" };
    case vscode.DiagnosticSeverity.Warning:
      return { type: "Warning" };
    case vscode.DiagnosticSeverity.Information:
      return { type: "Information" };
    case vscode.DiagnosticSeverity.Hint:
      return { type: "Hint" };
    default:
      return { type: "Error" };
  }
}

/** Convert a VS Code Diagnostic into the protocol Diagnostic shape. */
export function toProtocolDiagnostic(uri: vscode.Uri, diagnostic: vscode.Diagnostic): Diagnostic {
  return {
    path: uri.fsPath,
    range: {
      start: { line: diagnostic.range.start.line, character: diagnostic.range.start.character },
      end: { line: diagnostic.range.end.line, character: diagnostic.range.end.character },
    },
    severity: toProtocolDiagnosticSeverity(diagnostic.severity),
    message: diagnostic.message,
    ...(diagnostic.source ? { source: diagnostic.source } : {}),
  };
}

/** Extract IDE context snapshot from current editor and workspace documents. */
export function extractIdeContext(
  editor?: vscode.TextEditor,
  documents: readonly vscode.TextDocument[] = vscode.workspace.textDocuments,
  diagnosticsRevision = 0,
): IdeContextUpdate {
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
  for (const doc of documents) {
    if (doc.uri.scheme !== "file") continue;
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
  if (activeFile !== undefined) update.active_file = activeFile;
  if (selection !== undefined) update.selection = selection;
  if (openFiles.length > 0) update.open_files = openFiles;
  if (dirtyBuffers.length > 0) update.dirty_buffers = dirtyBuffers;
  return update;
}

/** Extract all diagnostics across open workspace documents or active document. */
export function extractDiagnostics(
  editor?: vscode.TextEditor,
  documents: readonly vscode.TextDocument[] = vscode.workspace.textDocuments,
): Diagnostic[] {
  const results: Diagnostic[] = [];
  const uris = new Set<string>();

  if (editor && editor.document.uri.scheme === "file") {
    uris.add(editor.document.uri.toString());
  }
  for (const doc of documents) {
    if (doc.uri.scheme === "file") {
      uris.add(doc.uri.toString());
    }
  }

  for (const uriString of uris) {
    const uri = vscode.Uri.parse(uriString);
    const diags = vscode.languages.getDiagnostics(uri);
    for (const d of diags) {
      results.push(toProtocolDiagnostic(uri, d));
    }
  }
  return results;
}

/** Find the most relevant diagnostic to fix for the current editor position or argument. */
export function findTargetDiagnostic(
  editor?: vscode.TextEditor,
  specificDiagnostic?: vscode.Diagnostic,
): Diagnostic | undefined {
  if (!editor || editor.document.uri.scheme !== "file") {
    return undefined;
  }
  const uri = editor.document.uri;

  if (specificDiagnostic) {
    return toProtocolDiagnostic(uri, specificDiagnostic);
  }

  const diagnostics = vscode.languages.getDiagnostics(uri);
  if (diagnostics.length === 0) {
    return undefined;
  }

  const sel = editor.selection;
  // If cursor or selection intersects a diagnostic, pick that one
  const intersecting = diagnostics.find((d) => {
    return (
      d.range.contains(sel.active) ||
      d.range.contains(sel.start) ||
      d.range.contains(sel.end) ||
      sel.contains(d.range.start) ||
      (d.range.start.line <= sel.end.line && d.range.end.line >= sel.start.line)
    );
  });

  if (intersecting) {
    return toProtocolDiagnostic(uri, intersecting);
  }

  // Otherwise, pick the first error, then warning, then any diagnostic in the file
  const firstError = diagnostics.find((d) => d.severity === vscode.DiagnosticSeverity.Error);
  if (firstError) {
    return toProtocolDiagnostic(uri, firstError);
  }
  const firstWarning = diagnostics.find((d) => d.severity === vscode.DiagnosticSeverity.Warning);
  if (firstWarning) {
    return toProtocolDiagnostic(uri, firstWarning);
  }
  return toProtocolDiagnostic(uri, diagnostics[0]);
}

/** Build full EditorActionContext for dispatching an editor action. */
export function buildEditorActionContext(
  editor?: vscode.TextEditor,
  workspaceFolders: readonly vscode.WorkspaceFolder[] = vscode.workspace.workspaceFolders ?? [],
  workspaceDocuments: readonly vscode.TextDocument[] = vscode.workspace.textDocuments,
  diagnosticsRevision = 0,
  repository?: string,
): EditorActionContext {
  const ide = extractIdeContext(editor, workspaceDocuments, diagnosticsRevision);
  const diagnostics = extractDiagnostics(editor, workspaceDocuments);
  const repositoryId = repository ?? workspaceFolders[0]?.uri.fsPath;

  const context: EditorActionContext = { ide };
  if (diagnostics.length > 0) {
    context.diagnostics = diagnostics;
  }
  if (repositoryId !== undefined) {
    context.repository_id = repositoryId;
  }
  return context;
}

export interface ActionExecutionOptions {
  repository?: string;
  model?: string;
  diagnosticsRevision?: number;
  workspaceFolders?: readonly vscode.WorkspaceFolder[];
  workspaceDocuments?: readonly vscode.TextDocument[];
}

/** Execute Explain Selection action. */
export function executeExplainSelection(
  client: DaemonClient,
  editor?: vscode.TextEditor,
  options?: ActionExecutionOptions,
): void {
  const context = buildEditorActionContext(
    editor,
    options?.workspaceFolders,
    options?.workspaceDocuments,
    options?.diagnosticsRevision ?? 0,
    options?.repository,
  );
  const action: EditorNativeAction = { type: "ExplainSelection" };
  client.runEditorAction(action, context, options?.model);
}

/** Execute Fix / Refactor Selection action. */
export function executeFixSelection(
  client: DaemonClient,
  editor?: vscode.TextEditor,
  options?: ActionExecutionOptions,
): void {
  const context = buildEditorActionContext(
    editor,
    options?.workspaceFolders,
    options?.workspaceDocuments,
    options?.diagnosticsRevision ?? 0,
    options?.repository,
  );
  const action: EditorNativeAction = { type: "FixSelection" };
  client.runEditorAction(action, context, options?.model);
}

/** Execute Fix Diagnostic action. */
export function executeFixDiagnostic(
  client: DaemonClient,
  editor?: vscode.TextEditor,
  specificDiagnostic?: vscode.Diagnostic,
  options?: ActionExecutionOptions,
): boolean {
  const target = findTargetDiagnostic(editor, specificDiagnostic);
  if (!target) {
    void vscode.window.showWarningMessage("Codypendent: No diagnostic found in active file to fix.");
    return false;
  }
  const context = buildEditorActionContext(
    editor,
    options?.workspaceFolders,
    options?.workspaceDocuments,
    options?.diagnosticsRevision ?? 0,
    options?.repository,
  );
  const action: EditorNativeAction = { type: "FixDiagnostic", diagnostic: target };
  client.runEditorAction(action, context, options?.model);
  return true;
}

/** Execute Generate Tests for Selection action. */
export function executeGenerateTestsForSelection(
  client: DaemonClient,
  editor?: vscode.TextEditor,
  options?: ActionExecutionOptions,
): void {
  const context = buildEditorActionContext(
    editor,
    options?.workspaceFolders,
    options?.workspaceDocuments,
    options?.diagnosticsRevision ?? 0,
    options?.repository,
  );
  const action: EditorNativeAction = { type: "GenerateTestsForSelection" };
  client.runEditorAction(action, context, options?.model);
}

/** Execute Review / Document Current File action. */
export function executeReviewCurrentFile(
  client: DaemonClient,
  editor?: vscode.TextEditor,
  options?: ActionExecutionOptions,
): void {
  const context = buildEditorActionContext(
    editor,
    options?.workspaceFolders,
    options?.workspaceDocuments,
    options?.diagnosticsRevision ?? 0,
    options?.repository,
  );
  const action: EditorNativeAction = { type: "ReviewCurrentFile" };
  client.runEditorAction(action, context, options?.model);
}

/**
 * Register all 5 editor-native actions as VS Code commands.
 */
export function registerEditorActions(
  context: vscode.ExtensionContext,
  getClient: () => DaemonClient | undefined,
  getDiagnosticsRevision: () => number = () => 0,
  getRepository: () => string | undefined = () => undefined,
): vscode.Disposable[] {
  function ensureClient(): DaemonClient | undefined {
    const client = getClient();
    if (!client) {
      void vscode.window.showWarningMessage("Codypendent: not attached to a session.");
      return undefined;
    }
    return client;
  }

  const disposables: vscode.Disposable[] = [
    // 1. Explain Selection
    vscode.commands.registerCommand("codypendent.explain", async () => {
      const client = ensureClient();
      if (!client) return;
      executeExplainSelection(client, vscode.window.activeTextEditor, {
        diagnosticsRevision: getDiagnosticsRevision(),
        repository: getRepository(),
      });
      await vscode.commands.executeCommand("codypendent.sessionView.focus");
    }),
    vscode.commands.registerCommand("codypendent.explainSelection", async () => {
      await vscode.commands.executeCommand("codypendent.explain");
    }),

    // 2. Refactor / Fix Selection
    vscode.commands.registerCommand("codypendent.refactor", async () => {
      const client = ensureClient();
      if (!client) return;
      executeFixSelection(client, vscode.window.activeTextEditor, {
        diagnosticsRevision: getDiagnosticsRevision(),
        repository: getRepository(),
      });
      await vscode.commands.executeCommand("codypendent.sessionView.focus");
    }),
    vscode.commands.registerCommand("codypendent.fixSelection", async () => {
      await vscode.commands.executeCommand("codypendent.refactor");
    }),

    // 3. Fix Diagnostic
    vscode.commands.registerCommand("codypendent.fixDiagnostic", async (diagArg?: vscode.Diagnostic) => {
      const client = ensureClient();
      if (!client) return;
      const ran = executeFixDiagnostic(client, vscode.window.activeTextEditor, diagArg, {
        diagnosticsRevision: getDiagnosticsRevision(),
        repository: getRepository(),
      });
      if (ran) {
        await vscode.commands.executeCommand("codypendent.sessionView.focus");
      }
    }),

    // 4. Generate Tests for Selection
    vscode.commands.registerCommand("codypendent.generateTests", async () => {
      const client = ensureClient();
      if (!client) return;
      executeGenerateTestsForSelection(client, vscode.window.activeTextEditor, {
        diagnosticsRevision: getDiagnosticsRevision(),
        repository: getRepository(),
      });
      await vscode.commands.executeCommand("codypendent.sessionView.focus");
    }),
    vscode.commands.registerCommand("codypendent.generateTestsForSelection", async () => {
      await vscode.commands.executeCommand("codypendent.generateTests");
    }),

    // 5. Review / Document Current File
    vscode.commands.registerCommand("codypendent.reviewFile", async () => {
      const client = ensureClient();
      if (!client) return;
      executeReviewCurrentFile(client, vscode.window.activeTextEditor, {
        diagnosticsRevision: getDiagnosticsRevision(),
        repository: getRepository(),
      });
      await vscode.commands.executeCommand("codypendent.sessionView.focus");
    }),
    vscode.commands.registerCommand("codypendent.reviewCurrentFile", async () => {
      await vscode.commands.executeCommand("codypendent.reviewFile");
    }),
    vscode.commands.registerCommand("codypendent.document", async () => {
      await vscode.commands.executeCommand("codypendent.reviewFile");
    }),
  ];

  for (const disposable of disposables) {
    context.subscriptions.push(disposable);
  }
  return disposables;
}
