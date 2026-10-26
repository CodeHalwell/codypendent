import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { ArrowUpRight, BrainCircuit, UsersRound, Workflow } from "lucide-react";
import type { ConnectionStatus, RunActivity, TranscriptItem } from "../types.js";
import { renderMarkdown } from "../markdown.js";
import { describeActivity } from "../runActivity.js";
import type { DesktopView } from "./Navigation.js";
import type { QuestionOutcomeView } from "../types.js";
import { QuestionCard } from "./QuestionCard.js";
import { useCommandActions, type CommandActions } from "../useCommandActions.js";
import { useCommandFocus } from "../useCommandFocus.js";
import { CommandFeedbackNote } from "./CommandFeedbackNote.js";
import { canReviewApproval, type ApprovalGrant } from "../approval.js";
import { ApprovalConsent } from "./ApprovalConsent.js";
import { PatchContent, type ReadArtifact, type SaveArtifact } from "./ArtifactActions.js";
import type { OpenReviewFile } from "./PatchReview.js";
import "./Transcript.css";
import { GeneratedToolResult } from "../agui/GeneratedToolResult.js";
import { ActionReview, approvalRiskText } from "./ActionReview.js";
import { desktopToolName, jsonData, legacyProviderError, resultSummary, toolTitle } from "./toolPresentation.js";
import { sanitizeFailureText } from "../failure.js";
import { activityEntries } from "./activityEntries.js";
import { WorkActivity } from "./WorkActivity.js";
import { progressText } from "./progressText.js";
import { messageModelLabel } from "../modelPresentation.js";

interface TranscriptProps {
  recoveredRuns?: ReadonlySet<string>;
  focusDecision?: string | null;
  /** Changes only after a local objective is accepted; returns to its latest turn. */
  followLatestRequest?: number;
  items: TranscriptItem[];
  connectionStatus: ConnectionStatus;
  /** Connection diagnostic, also shown by the shared connection banner. */
  statusDetail?: string;
  onApprove?: (approvalId: string, scope?: ApprovalGrant) => void | Promise<void>;
  onReject?: (approvalId: string) => void | Promise<void>;
  /**
   * What the live run is doing, for the working row under the last item.
   * Absent (or `idle`) draws nothing; `streaming` draws nothing either, because
   * the growing reply is its own signal.
   */
  activity?: RunActivity;
  /**
   * Start a new run with the same objective, from a failure card. Absent when
   * nothing can be submitted (not connected), in which case the card offers
   * no Retry rather than a button that fails.
   */
  onRetry?: (objective: string) => void;
  /** Open a configuration surface a failure card points at. */
  onOpenView?: (view: DesktopView) => void;
  /** Answer or reject a parked question. Absent when nothing can be sent. */
  onResolveQuestion?: (questionId: string, outcome: QuestionOutcomeView) => void | Promise<void>;
  onReadArtifact?: ReadArtifact;
  onSaveArtifact?: SaveArtifact;
  onOpenFile?: OpenReviewFile;
  commandActions?: CommandActions;
}

const EMPTY_HEADING: Record<ConnectionStatus, string> = {
  disconnected: "Your workspace starts here",
  connecting: "Opening your workspace",
  connected: "Ready",
};

/**
 * How close to the bottom still counts as following the stream, in pixels.
 * Further up than this and the reader has deliberately scrolled back, so the
 * view stops chasing new output and leaves them where they are.
 */
const PINNED_SLACK_PX = 80;
export const TRANSCRIPT_PAGE_SIZE = 200;
type PageAnchor = { id: string; index: number };

// Styles are module constants rather than object literals in the render body.
// A fresh style object per row per frame is allocation the streaming path would
// otherwise repeat for every item in the transcript, on every token.
const LIST: React.CSSProperties = {
  flex: 1,
  overflowY: "auto",
  minHeight: 0,
  display: "flex",
  flexDirection: "column",
  gap: 20,
};
const EMPTY_WRAP: React.CSSProperties = { margin: "auto", textAlign: "center", color: "var(--cody-text-faint)" };
const ROW_USER: React.CSSProperties = {
  alignSelf: "flex-end",
  maxWidth: "85%",
  background: "var(--cody-inset)",
  padding: "13px 18px",
  borderRadius: "16px 16px 4px 16px",
  fontSize: 14,
  lineHeight: 1.5,
};
const ROW_ASSISTANT: React.CSSProperties = {
  alignSelf: "flex-start",
  width: "100%",
  padding: "6px 0 12px",
  fontSize: 15,
  lineHeight: 1.75,
  color: "var(--cody-text)",
  // No `whiteSpace: pre-wrap` here: the Markdown renderer sets it per
  // paragraph, and setting it on the row as well collapses its block spacing
  // into the raw newlines it was meant to replace.
  overflowWrap: "anywhere",
};
const ROW_THOUGHT: React.CSSProperties = {
  alignSelf: "flex-start",
  maxWidth: "100%",
  borderLeft: "2px solid var(--cody-border-strong)",
  padding: "6px 12px",
  fontSize: 12,
  color: "var(--cody-text-muted)",
};
const THOUGHT_SUMMARY: React.CSSProperties = { cursor: "pointer", fontWeight: 500 };
const ROW_TOOL: React.CSSProperties = {
  alignSelf: "flex-start",
  width: "100%",
  background: "var(--cody-panel)",
  border: "1px solid var(--cody-border)",
  borderRadius: 12,
  overflow: "hidden",
};
const TOOL_HEAD: React.CSSProperties = {
  background: "var(--cody-panel-raised)",
  padding: "11px 14px",
  fontSize: 12,
  fontWeight: 600,
  color: "var(--cody-link)",
  display: "flex",
  justifyContent: "space-between",
  alignItems: "flex-start",
  gap: 12,
  borderBottom: "1px solid var(--cody-border)",
};
const TOOL_LABEL: React.CSSProperties = { minWidth: 0, overflowWrap: "anywhere" };
const TOOL_KIND: React.CSSProperties = { display: "block", marginTop: 3, fontWeight: 400, fontSize: 11, color: "var(--cody-text-muted)" };
const TOOL_BODY: React.CSSProperties = { padding: "8px 12px", fontSize: 13, color: "var(--cody-text)" };
const TOOL_ARGS: React.CSSProperties = {
  margin: 0,
  padding: 12,
  fontSize: 12,
  overflowX: "auto",
  color: "var(--cody-text-muted)",
};
const ROW_QUESTION: React.CSSProperties = {
  alignSelf: "flex-start",
  width: "100%",
  background: "var(--cody-panel-hover)",
  border: "1px solid var(--cody-accent)",
  borderRadius: 8,
  padding: 16,
};
const QUESTION_TITLE: React.CSSProperties = {
  fontWeight: 600,
  color: "var(--cody-link)",
  marginBottom: 8,
  fontSize: 14,
};
const QUESTION_BODY: React.CSSProperties = { fontSize: 13, color: "var(--cody-text)" };
const ROW_APPROVAL: React.CSSProperties = {
  boxSizing: "border-box",
  overflowWrap: "anywhere",
  alignSelf: "flex-start",
  width: "100%",
  background: "var(--cody-warning-bg)",
  border: "1px solid var(--cody-warning-border)",
  borderRadius: 8,
  padding: 16,
};
const APPROVAL_TITLE: React.CSSProperties = {
  fontWeight: 600,
  color: "var(--cody-warning)",
  marginBottom: 8,
  fontSize: 14,
};
const APPROVAL_BODY: React.CSSProperties = { fontSize: 13, color: "var(--cody-text)", marginBottom: 12 };
const APPROVAL_ACTIONS: React.CSSProperties = { display: "flex", gap: 8 };
const APPROVE_BUTTON: React.CSSProperties = {
  background: "var(--cody-success-strong)",
  border: "none",
  color: "var(--cody-on-accent)",
  padding: "6px 12px",
  borderRadius: 6,
  fontSize: 12,
  cursor: "pointer",
  fontWeight: 600,
};
const REJECT_BUTTON: React.CSSProperties = {
  background: "var(--cody-inset)",
  border: "1px solid var(--cody-border-strong)",
  color: "var(--cody-text-secondary)",
  padding: "6px 12px",
  borderRadius: 6,
  fontSize: 12,
  cursor: "pointer",
};
const SYSTEM_ROW: React.CSSProperties = { fontSize: 12, color: "var(--cody-text-muted)", textAlign: "center" };
const SYSTEM_ROW_INFO: React.CSSProperties = { ...SYSTEM_ROW, color: "var(--cody-link-soft)" };
const SYSTEM_ROW_WARNING: React.CSSProperties = {
  ...SYSTEM_ROW,
  color: "var(--cody-warning-text)",
  alignSelf: "stretch",
  textAlign: "left",
  border: "1px solid var(--cody-warning-border)",
  background: "var(--cody-warning-bg)",
  borderRadius: 6,
  padding: "6px 10px",
};
const ROW_FAILURE: React.CSSProperties = {
  alignSelf: "stretch",
  background: "var(--cody-danger-bg)",
  border: "1px solid var(--cody-danger)",
  borderRadius: 8,
  padding: "12px 16px",
};
const FAILURE_TITLE: React.CSSProperties = {
  fontWeight: 600,
  color: "var(--cody-danger-text)",
  fontSize: 14,
  marginBottom: 4,
  overflowWrap: "anywhere",
};
const FAILURE_HINT: React.CSSProperties = { fontSize: 13, color: "var(--cody-text)", marginBottom: 10 };
const FAILURE_ACTIONS: React.CSSProperties = { display: "flex", gap: 8, flexWrap: "wrap" };
const FAILURE_DETAIL: React.CSSProperties = {
  marginTop: 10,
  fontSize: 12,
  color: "var(--cody-text-secondary)",
};
const FAILURE_DETAIL_BODY: React.CSSProperties = {
  marginTop: 6,
  whiteSpace: "pre-wrap",
  fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
  fontSize: 11,
  overflowWrap: "anywhere",
  maxHeight: 320,
  overflowY: "auto",
};
const FAILURE_BUTTON: React.CSSProperties = {
  background: "var(--cody-inset)",
  border: "1px solid var(--cody-border-strong)",
  color: "var(--cody-text)",
  padding: "6px 12px",
  borderRadius: 6,
  fontSize: 12,
  cursor: "pointer",
  fontWeight: 600,
};
const FAILURE_PRIMARY: React.CSSProperties = {
  ...FAILURE_BUTTON,
  background: "var(--cody-success-strong)",
  border: "none",
  color: "var(--cody-on-accent)",
};
const WORKING_ROW: React.CSSProperties = {
  alignSelf: "flex-start",
  display: "flex",
  alignItems: "center",
  gap: 8,
  fontSize: 12,
  color: "var(--cody-text-muted)",
  padding: "2px 4px",
};
const WORKING_DOT: React.CSSProperties = {
  width: 8,
  height: 8,
  borderRadius: "50%",
  background: "var(--cody-warning)",
  animation: "cody-pulse 1.2s ease-in-out infinite",
};
/** Mirrors the TUI's `NOTE_INLINE_LINE_THRESHOLD`: past this, a note folds. */
const SYSTEM_INLINE_LINE_LIMIT = 2;
/** A single line this long is a wall too, however few newlines it contains. */
const SYSTEM_INLINE_CHARS = 160;
const SYSTEM_FOLD: React.CSSProperties = {
  alignSelf: "stretch",
  fontSize: 12,
  color: "var(--cody-text-muted)",
  border: "1px dashed var(--cody-inset)",
  borderRadius: 6,
  padding: "6px 10px",
};
const SYSTEM_FOLD_SUMMARY: React.CSSProperties = { cursor: "pointer", userSelect: "none" };
const SYSTEM_FOLD_BODY: React.CSSProperties = {
  marginTop: 8,
  whiteSpace: "pre-wrap",
  fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
  fontSize: 11,
  overflowWrap: "anywhere",
};
const ROW_BACKSTAGE: React.CSSProperties = {
  alignSelf: "stretch",
  background: "transparent",
  border: "1px dashed var(--cody-inset)",
  borderRadius: 6,
  padding: "6px 10px",
  fontSize: 11,
  color: "var(--cody-text-faint)",
};
const BACKSTAGE_SUMMARY: React.CSSProperties = { cursor: "pointer", userSelect: "none" };
const BACKSTAGE_BODY: React.CSSProperties = {
  marginTop: 8,
  whiteSpace: "pre-wrap",
  fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
  fontSize: 11,
  maxHeight: 320,
  overflowY: "auto",
};

interface RowProps {
  item: TranscriptItem;
  failedRun?: boolean;
  recoveredRun?: boolean;
  onApprove?: (approvalId: string, scope?: ApprovalGrant) => void | Promise<void>;
  onReject?: (approvalId: string) => void | Promise<void>;
  onRetry?: (objective: string) => void;
  onOpenView?: (view: DesktopView) => void;
  onResolveQuestion?: (questionId: string, outcome: QuestionOutcomeView) => void | Promise<void>;
  onReadArtifact?: ReadArtifact;
  onSaveArtifact?: SaveArtifact;
  onOpenFile?: OpenReviewFile;
  commandActions?: CommandActions;
}

/** The buttons a failure card offers, from the remedy the failure text gave. */
function failureActions(
  item: TranscriptItem,
  onRetry: RowProps["onRetry"],
  onOpenView: RowProps["onOpenView"],
): React.ReactNode[] {
  const actions: React.ReactNode[] = [];
  const acp = /\bacp\b/i.test(`${item.text} ${item.failureDetail ?? ""}`);
  if (item.remedy === "keys" && onOpenView) {
    actions.push(
      <button key="keys" style={FAILURE_PRIMARY} onClick={() => onOpenView(acp ? "providers" : "keys")}>
        {acp ? "Open providers" : "Open API keys"}
      </button>,
    );
  }
  if (item.remedy === "models" && onOpenView) {
    actions.push(
      <button key="models" style={FAILURE_PRIMARY} onClick={() => onOpenView("models")}>
        Choose a model
      </button>,
    );
  }
  if (item.objective && onRetry) {
    const objective = item.objective;
    actions.push(
      <button
        key="retry"
        style={item.remedy === "retry" || item.remedy === "none" ? FAILURE_PRIMARY : FAILURE_BUTTON}
        onClick={() => onRetry(objective)}
      >
        Retry
      </button>,
    );
  }
  return actions;
}

function failureHint(item: TranscriptItem): string | undefined {
  const detail = `${item.text} ${item.failureDetail ?? ""}`;
  if (!/\bacp\b/i.test(detail)) return item.hint;
  if (item.remedy === "keys") return "Sign in to the installed agent, then retry.";
  if (/timeout|timed out/i.test(detail)) return "The installed agent did not finish responding. Retry the task.";
  return item.hint;
}

function ApprovalCard({ item, onApprove, onReject, commandActions }: RowProps) {
  const actionKey = `approval:${item.approvalId}:${item.decisionRevision ?? item.id}`;
  const localActions = useCommandActions(`${actionKey}:${Boolean(onApprove || onReject)}`);
  const actions = commandActions ?? localActions;
  const result = actions.feedback[actionKey];
  const { ref, preserveFocus } = useCommandFocus(result);
  const locked = result?.phase === "pending" || result?.phase === "accepted";
  const decide = (command: RowProps["onApprove"], scope?: ApprovalGrant) => {
    if (!command || !item.approvalId || actions.isLocked(actionKey)) return;
    preserveFocus();
    actions.run(actionKey, () => scope ? command(item.approvalId!, scope) : command(item.approvalId!), true);
  };
  return (
    <div ref={ref} style={ROW_APPROVAL} role="group" aria-label="Approval required" tabIndex={-1}
      data-decision-id={`Approval:${item.approvalId}`} aria-busy={result?.phase === "pending"}>
      <div style={APPROVAL_TITLE}>Approval Required</div>
      <ActionReview action={item.approvalAction} fallback={item.text} />
      {item.approvalRisk && <div style={APPROVAL_BODY}>
        Risk: {item.approvalRisk.level.type}
        {(item.approvalRisk.reasons ?? []).filter((reason) => approvalRiskText(reason)).map((reason, i) => <div key={i}>{reason}</div>)}
        {(item.approvalRisk.reasons ?? []).some((reason) => !approvalRiskText(reason)) && <details className="transcript-technical-details">
          <summary>Policy details</summary>
          <pre>{JSON.stringify(item.approvalRisk, null, 2)}</pre>
        </details>}
      </div>}
      {!canReviewApproval(item.approvalAction) && <div role="status" style={APPROVAL_BODY}>
        This client cannot review this action. Reject it or update Codypendent before approving.
      </div>}
      {item.approvalId && (
        <div style={APPROVAL_ACTIONS}>
          <button disabled={locked || !onApprove || !canReviewApproval(item.approvalAction)} onClick={() => decide(onApprove)} style={APPROVE_BUTTON}>
            Approve
          </button>
          <ApprovalConsent action={item.approvalAction} pattern={item.approvalPattern} disabled={locked || !onApprove}
            onChoose={(scope) => decide(onApprove, scope)} />
          <button disabled={locked || !onReject} onClick={() => decide(onReject)} style={REJECT_BUTTON}>
            Reject
          </button>
        </div>
      )}
      <CommandFeedbackNote feedback={result} />
    </div>
  );
}

/** Closed patch cards do not parse or mount diffs; an opened card keeps its loaded artifact. */
function PatchCard({ item, onReadArtifact, onSaveArtifact, onOpenFile }: RowProps) {
  const [visited, setVisited] = useState(false);
  return <details style={SYSTEM_FOLD} onToggle={(event) => { if (event.currentTarget.open) setVisited(true); }}>
    <summary style={SYSTEM_FOLD_SUMMARY}>{item.text}</summary>
    {visited && <div style={SYSTEM_FOLD_BODY}>
      <PatchContent key={item.artifact?.id ?? item.id} preview={item.diffPreview} files={item.patchFiles} runId={item.patchRunId}
        onOpenFile={onOpenFile} artifact={item.artifact} onRead={onReadArtifact} onSave={onSaveArtifact} />
    </div>}
  </details>;
}

/**
 * One transcript row, memoized.
 *
 * A streaming reply replaces ONLY the last item of the transcript array; every
 * earlier item keeps its object identity for the whole stream. Without this
 * boundary React reconciles the entire transcript once per token, which
 * measured at 4.29 ms per token against a 400-item history — linear in the
 * history, so the longer a session runs the further behind the stream the UI
 * falls.
 *
 * `onApprove`/`onReject` must be referentially stable at the call site or the
 * memo never hits and this is decoration.
 */
function ResponseModel({ modelId }: { modelId?: string }) {
  return <div className="transcript-model" aria-label={`Response model: ${messageModelLabel(modelId)}`}
    data-model-id={modelId} title={modelId ? `Recorded by daemon: ${modelId}` : "The daemon did not record a model identity for this message."}>
    <span>{messageModelLabel(modelId)}</span><span className="transcript-model-source">Daemon</span>
  </div>;
}

const TranscriptRow = React.memo(function TranscriptRow({
  item,
  failedRun,
  recoveredRun,
  onApprove,
  onReject,
  onRetry,
  onOpenView,
  onResolveQuestion,
  onReadArtifact,
  onSaveArtifact,
  onOpenFile,
  commandActions,
}: RowProps) {
  switch (item.type) {
    case "failure":
      if (recoveredRun) return <details style={SYSTEM_FOLD}>
        <summary style={SYSTEM_FOLD_SUMMARY}>Earlier model connection attempt</summary>
        <div style={SYSTEM_FOLD_BODY}><p>{item.text}</p>{item.failureDetail && <pre>{item.failureDetail}</pre>}</div>
      </details>;
      // The run failed. This is the card the operator has to find, so it is
      // loud, it says what to do next, and the full sanitised chain is one
      // click away rather than dumped inline. Mirrors the TUI's failure card
      // and its Alt-R / Alt-A / Alt-M chips (`crates/tui/src/render.rs`).
      return (
        <div style={ROW_FAILURE} role="alert" data-testid="run-failure">
          <div style={FAILURE_TITLE}>Run failed: {item.text}</div>
          {failureHint(item) && <div style={FAILURE_HINT}>{failureHint(item)}</div>}
          <div style={FAILURE_ACTIONS}>{failureActions(item, onRetry, onOpenView)}</div>
          {item.failureDetail && item.failureDetail !== item.text && (
            <details style={FAILURE_DETAIL}>
              <summary style={{ cursor: "pointer" }}>Full error</summary>
              <div style={FAILURE_DETAIL_BODY}>{item.failureDetail}</div>
            </details>
          )}
        </div>
      );

    case "user":
      return <div style={ROW_USER} className="transcript-user">{item.text}</div>;

    case "assistant":
      if (failedRun && legacyProviderError(item.text)) return <details style={ROW_THOUGHT} className="transcript-provider-diagnostic">
        <summary style={THOUGHT_SUMMARY}>Provider error details</summary>
        <pre style={FAILURE_DETAIL_BODY}>{sanitizeFailureText(item.text)}</pre>
      </details>;
      if (/^\s*Warning:\s*(Model\s+metadata|Skill\s+descriptions)\b/i.test(item.text) && legacyProviderError(item.text)) return <div style={ROW_FAILURE} role="alert">
        <div style={FAILURE_TITLE}>Provider request failed</div>
        <div style={FAILURE_HINT}>{legacyProviderError(item.text)}</div>
        <details className="transcript-technical-details"><summary>Technical details</summary><pre>{sanitizeFailureText(item.text)}</pre></details>
      </div>;
      // The model writes Markdown; showing it raw meant reading `##` and `**`
      // as literal characters. Rendered to React elements only — never HTML —
      // so model output cannot inject into the webview.
      return <div style={ROW_ASSISTANT} className="transcript-assistant">
        <ResponseModel modelId={item.modelId} />
        {renderMarkdown(item.text, { onOpenSource: onOpenFile ? (file, line) => onOpenFile(file, line, item.runId) : undefined })}
      </div>;

    case "thought":
      return item.text.trim() ? <div style={ROW_THOUGHT} className="transcript-thought" aria-label="Agent progress">
        <ResponseModel modelId={item.modelId} />
        {renderMarkdown(progressText(item.text), { onOpenSource: onOpenFile ? (file, line) => onOpenFile(file, line, item.runId) : undefined })}
      </div> : null;

    case "tool_call": {
      const rawLabel = item.toolLabel ?? item.toolName ?? "Tool";
      const label = jsonData(rawLabel) === undefined ? toolTitle(rawLabel) : toolTitle(item.toolName ?? "Agent action");
      const kind = desktopToolName(rawLabel) ? "Workspace tool" : item.toolKind ?? (item.toolName?.startsWith("acp:") ? undefined : item.toolName);
      const technicalKind = Boolean(kind?.startsWith("desktop.") || kind === "generateSandboxedUi" || (kind && jsonData(kind) !== undefined));
      const structuredText = jsonData(item.text);
      const output = typeof item.toolResult === "string" ? jsonData(item.toolResult) : item.toolResult;
      const plainError = item.status === "error" && typeof item.toolResult === "string" && output === undefined;
      const hasDetails = item.toolArgs !== undefined || item.toolResult !== undefined || structuredText !== undefined || label !== rawLabel || technicalKind;
      return (
        <div style={ROW_TOOL} className="transcript-tool">
          <div style={TOOL_HEAD} data-tool-header>
            <div style={TOOL_LABEL}>
              <span>{label}</span>
              {kind && kind !== label && !technicalKind && <span style={TOOL_KIND}>{kind}</span>}
            </div>
            <span
              style={{
                flexShrink: 0,
                // An error used to paint in the SUCCESS green (the ternary
                // only distinguished "running"), so a failed call read as a
                // finished one.
                color:
                  item.status === "running"
                    ? "var(--cody-warning)"
                    : item.status === "error"
                      ? "var(--cody-danger-soft)"
                      : item.status === "finished"
                        ? "var(--cody-text-muted)"
                        : "var(--cody-success)",
              }}
            >
              {item.status === "finished" ? "finished · result unavailable" : item.status ?? "running"}
            </span>
          </div>
          {item.text && item.text !== rawLabel && item.text !== kind && structuredText === undefined ? <div style={TOOL_BODY}>{item.text}</div> : null}
          {plainError ? <div style={{ ...TOOL_BODY, color: "var(--cody-danger-soft)" }}>{item.toolResult as string}</div>
            : item.toolResult !== undefined && <div style={TOOL_BODY}>{item.status === "error" ? "The action failed. Open the technical details for its response." : resultSummary(output ?? item.toolResult)}</div>}
          {hasDetails && <details className="transcript-technical-details" style={TOOL_BODY}>
            <summary>Tool details</summary>
            {item.toolName && <div>{item.toolName}</div>}
            {rawLabel !== label && rawLabel !== item.toolName && <div>{rawLabel}</div>}
            {item.toolKind && item.toolKind !== kind && <div>Original tool type: {item.toolKind}</div>}
            {item.toolArgs !== undefined && <><strong>Arguments</strong><pre style={TOOL_ARGS}>{JSON.stringify(item.toolArgs, null, 2)}</pre></>}
            {structuredText !== undefined && <pre style={TOOL_ARGS}>{item.text}</pre>}
            {item.toolResult !== undefined && <><strong>Response</strong><pre style={TOOL_ARGS}>{typeof item.toolResult === "string" ? item.toolResult : JSON.stringify(output, null, 2)}</pre></>}
          </details>}
          {item.artifact?.media_type === "application/vnd.codypendent.desktop-tool+json" &&
            <GeneratedToolResult artifact={item.artifact} read={onReadArtifact} save={onSaveArtifact} />}
        </div>
      );
    }

    case "tool_result": {
      const data = jsonData(item.text);
      return <div style={ROW_TOOL} className="transcript-tool">
        <div style={TOOL_BODY}>{data === undefined ? resultSummary(item.text) : resultSummary(data)}</div>
        <details className="transcript-technical-details" style={TOOL_BODY}><summary>Tool details</summary><pre style={TOOL_ARGS}>{item.text}</pre></details>
      </div>;
    }

    case "question":
      // Answerable when the daemon named the question and sent its prompts;
      // an older store shape (text only) keeps the read-only card.
      if (item.questionId && item.questionPrompts && item.questionPrompts.length > 0) {
        return (
          <QuestionCard
            questionId={item.questionId}
            prompts={item.questionPrompts}
            revision={item.decisionRevision}
            commandActions={commandActions}
            onResolve={onResolveQuestion}
          />
        );
      }
      return (
        <div style={ROW_QUESTION}>
          <div style={QUESTION_TITLE}>Question</div>
          <div style={QUESTION_BODY}>{item.text}</div>
        </div>
      );

    case "approval":
      return <ApprovalCard item={item} onApprove={onApprove} onReject={onReject} commandActions={commandActions} />;

    case "backstage": {
      // The context manifest and curated-memory writes: real, but not the
      // conversation. One dim expandable line per run, mirroring the TUI's
      // `TranscriptEntry::Backstage`. Printing the manifest inline buried the
      // answer under a screenful of tool descriptions.
      const contextLines = item.contextLines;
      const memoryUpdates = item.memoryUpdates ?? 0;
      const parts: string[] = [];
      if (contextLines !== undefined) {
        parts.push(`${contextLines} context line${contextLines === 1 ? "" : "s"}`);
      }
      if (memoryUpdates > 0) {
        parts.push(`${memoryUpdates} memory update${memoryUpdates === 1 ? "" : "s"}`);
      }
      return (
        <details style={ROW_BACKSTAGE}>
          <summary style={BACKSTAGE_SUMMARY}>
            Backstage — {parts.length > 0 ? parts.join(" · ") : "run context"}
          </summary>
          <div style={BACKSTAGE_BODY}>{(item.raw ?? []).join("\n\n")}</div>
        </details>
      );
    }

    default: {
      if (item.type === "system" && item.text.trim() === "context carried from the conversation") return null;
      const providerError = item.type === "system" ? legacyProviderError(item.text) : null;
      if (providerError) return <div style={ROW_FAILURE} role="alert">
        <div style={FAILURE_TITLE}>Provider request failed</div>
        <div style={FAILURE_HINT}>{providerError}</div>
        <details className="transcript-technical-details"><summary>Technical details</summary><pre>{sanitizeFailureText(item.text)}</pre></details>
      </div>;
      // A proposed patch renders as a reviewable card: the touched files and
      // the wire's bounded diff preview, folded — not just an artifact id.
      if (item.artifact || item.diffPreview || (item.patchFiles && item.patchFiles.length > 0)) {
        return <PatchCard item={item} onReadArtifact={onReadArtifact} onSaveArtifact={onSaveArtifact} onOpenFile={onOpenFile} />;
      }
      // A long system note folds, exactly as the TUI folds one past
      // `NOTE_INLINE_LINE_THRESHOLD`. The daemon's worktree-retention note is
      // the case that prompted this: full paths plus the `git worktree remove`
      // and `git branch -D` commands, which is real recovery information and so
      // must not be discarded — but printed inline it buried the conversation
      // under a wall of plumbing. One line, with the rest a click away.
      const lines = item.text.split("\n");
      if (lines.length > SYSTEM_INLINE_LINE_LIMIT || item.text.length > SYSTEM_INLINE_CHARS) {
        const summary = lines.find((line) => line.trim() !== "") ?? item.text;
        return (
          <details style={SYSTEM_FOLD}>
            <summary style={SYSTEM_FOLD_SUMMARY}>
              {summary.length > SYSTEM_INLINE_CHARS
                ? `${summary.slice(0, SYSTEM_INLINE_CHARS)}…`
                : summary}
            </summary>
            <div style={SYSTEM_FOLD_BODY}>{item.text}</div>
          </details>
        );
      }
      return (
        <div
          style={
            item.tone === "warning"
              ? SYSTEM_ROW_WARNING
              : item.tone === "info"
                ? SYSTEM_ROW_INFO
                : SYSTEM_ROW
          }
          role={item.tone === "warning" ? "status" : undefined}
        >
          {item.text}
        </div>
      );
    }
  }
});

export const Transcript: React.FC<TranscriptProps> = ({
  items,
  recoveredRuns,
  connectionStatus,
  focusDecision,
  followLatestRequest = 0,
  onApprove,
  onReject,
  activity,
  onRetry,
  onOpenView,
  onResolveQuestion,
  onReadArtifact,
  onSaveArtifact,
  onOpenFile,
  commandActions,
}) => {
  const working = activity ? describeActivity(activity) : null;
  const failedRuns = new Set(items.filter((item) => item.type === "failure" && item.runId).map((item) => item.runId));
  const listRef = useRef<HTMLDivElement>(null);
  /** Whether the reader is still following the bottom of the stream. */
  const pinned = useRef(true);
  /** In-flight rAF handle, so many tokens in one frame coalesce into one scroll. */
  const scrollFrame = useRef(0);
  // Null follows the latest messages. An older page is anchored by item ID so
  // live appends and decisions resolved before it do not move the reader.
  const [page, setPage] = useState<PageAnchor | null>(null);
  const pageScroll = useRef<"top" | "bottom" | null>(null);
  const previousFollowLatestRequest = useRef(followLatestRequest);
  const lastStart = Math.max(0, items.length - TRANSCRIPT_PAGE_SIZE);
  const anchorIndex = page
    ? items[page.index]?.id === page.id ? page.index : items.findIndex((item) => item.id === page.id)
    : -1;
  // If the anchor itself resolves, keep its former position rather than
  // jumping to live output. Clamp when a refreshed history becomes shorter.
  const start = page === null ? lastStart : Math.min(anchorIndex < 0 ? page.index : anchorIndex, lastStart);
  const end = Math.min(items.length, start + TRANSCRIPT_PAGE_SIZE);

  useLayoutEffect(() => {
    if (page && (page.index !== start || page.id !== items[start]?.id)) {
      const replacement = items[start];
      setPage(replacement ? { id: replacement.id, index: start } : null);
    }
  }, [page, start, items]);

  useLayoutEffect(() => {
    if (previousFollowLatestRequest.current === followLatestRequest) return;
    previousFollowLatestRequest.current = followLatestRequest;
    cancelAnimationFrame(scrollFrame.current);
    scrollFrame.current = 0;
    pinned.current = true;
    pageScroll.current = "bottom";
    setPage(null);
    const el = listRef.current;
    if (page === null && el) {
      el.scrollTop = el.scrollHeight;
      pageScroll.current = null;
    }
  }, [followLatestRequest, page]);

  useLayoutEffect(() => {
    // WebKit can defer animation frames while a restored window opens. Place
    // new rows before paint when following, including rows arriving after a
    // local Send was accepted. Token updates keep the coalesced frame below.
    if (page === null && pinned.current && !focusDecision) {
      const el = listRef.current;
      if (el) el.scrollTop = el.scrollHeight;
    }
  }, [items.length, page, focusDecision]);

  const showPage = (index: number | null) => {
    cancelAnimationFrame(scrollFrame.current);
    scrollFrame.current = 0;
    pinned.current = index === null;
    pageScroll.current = index === null ? "bottom" : "top";
    setPage(index === null ? null : { id: items[index].id, index });
  };

  useLayoutEffect(() => {
    const el = listRef.current;
    if (el && pageScroll.current) {
      el.scrollTop = pageScroll.current === "bottom" ? el.scrollHeight : 0;
      pageScroll.current = null;
    }
  }, [page, items.length]);

  const handleScroll = useCallback(() => {
    const el = listRef.current;
    if (!el) {
      return;
    }
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight <= PINNED_SLACK_PX;
    pinned.current = atBottom && end === items.length;
    if (pinned.current && page !== null) {
      setPage(null);
    } else if (!atBottom && page === null && items.length > TRANSCRIPT_PAGE_SIZE) {
      setPage({ id: items[start].id, index: start });
    }
  }, [page, start, end, items]);

  const focusedDecision = useRef<string | null>(null);
  useEffect(() => {
    if (!focusDecision) { focusedDecision.current = null; return; }
    if (focusedDecision.current === focusDecision) return;
    const index = items.findIndex((item) => `Approval:${item.approvalId}` === focusDecision || `Question:${item.questionId}` === focusDecision);
    if (index >= 0 && (index < start || index >= end)) {
      showPage(Math.min(Math.floor(index / TRANSCRIPT_PAGE_SIZE) * TRANSCRIPT_PAGE_SIZE, lastStart));
      return;
    }
    const card = Array.from(listRef.current?.querySelectorAll<HTMLElement>("[data-decision-id]") ?? [])
      .find((element) => element.dataset.decisionId === focusDecision);
    if (!card) return; // Catch-up may still be supplying the requested card.
    focusedDecision.current = focusDecision;
    pinned.current = false;
    if (page === null && items.length > TRANSCRIPT_PAGE_SIZE) {
      setPage({ id: items[start].id, index: start });
    }
    cancelAnimationFrame(scrollFrame.current);
    scrollFrame.current = 0;
    card.focus({ preventScroll: true });
    card.scrollIntoView?.({ block: "center" });
  }, [focusDecision, items, start, end]);

  useEffect(() => {
    // Two things this must NOT do. It must not scroll when the reader has
    // scrolled up to read something — the previous version yanked them back to
    // the bottom on every token. And it must not use `scrollIntoView` with
    // smooth behaviour: `items` is a new array per token, so that started and
    // abandoned an animation for every token of every reply, forcing layout
    // each time and never once completing. Setting `scrollTop` is instant, and
    // the rAF gate means at most one per frame however fast the stream runs.
    if (!pinned.current || scrollFrame.current !== 0) {
      return;
    }
    scrollFrame.current = requestAnimationFrame(() => {
      scrollFrame.current = 0;
      const el = listRef.current;
      if (el && pinned.current) {
        el.scrollTop = el.scrollHeight;
      }
    });
  }, [items, working]);

  useEffect(
    () => () => {
      if (scrollFrame.current !== 0) {
        cancelAnimationFrame(scrollFrame.current);
      }
    },
    [],
  );

  const renderItem = (item: TranscriptItem) => <TranscriptRow
    key={item.id}
    item={item}
    failedRun={Boolean(item.runId && failedRuns.has(item.runId))}
    recoveredRun={Boolean(item.runId && recoveredRuns?.has(item.runId))}
    onApprove={onApprove}
    onReject={onReject}
    onRetry={onRetry}
    onOpenView={onOpenView}
    onResolveQuestion={onResolveQuestion}
    onReadArtifact={onReadArtifact}
    onSaveArtifact={onSaveArtifact}
    onOpenFile={onOpenFile}
    commandActions={commandActions}
  />;

  return (
    <>
      {items.length > TRANSCRIPT_PAGE_SIZE && (
        <nav className="transcript-history" aria-label="Conversation history">
          <span role="status">Messages {(start + 1).toLocaleString()}–{end.toLocaleString()} of {items.length.toLocaleString()}</span>
          <div>
            <button type="button" disabled={start === 0} onClick={() => showPage(0)}>First</button>
            <button type="button" disabled={start === 0} onClick={() => showPage(Math.max(0, start - TRANSCRIPT_PAGE_SIZE))}>Older</button>
            <button type="button" disabled={end === items.length} onClick={() => showPage(start + TRANSCRIPT_PAGE_SIZE >= lastStart ? null : start + TRANSCRIPT_PAGE_SIZE)}>Newer</button>
            <button type="button" disabled={page === null} onClick={() => showPage(null)}>Latest</button>
          </div>
        </nav>
      )}
    <div ref={listRef} onScroll={handleScroll} style={LIST} className="transcript-list" data-transcript-window={`${start}:${end}`}>
      {focusDecision && focusedDecision.current !== focusDecision && !items.some((item) => `Approval:${item.approvalId}` === focusDecision || `Question:${item.questionId}` === focusDecision) && (
        <p role="status" style={{ color: "var(--cody-text-muted)", fontSize: 13 }}>This request is not in the session’s loaded history. It may already be resolved.</p>
      )}
      {items.length === 0 ? (
        <div style={EMPTY_WRAP} className="workspace-welcome">
          <img src="/codypendent-mark.png" alt="" width="52" height="52" className="workspace-welcome-mark" />
          <p className="workspace-welcome-eyebrow">Codypendent</p>
          <h2>Your workspace. Your agents.</h2>
          {connectionStatus !== "connected" && <h3 className="workspace-welcome-state">{EMPTY_HEADING[connectionStatus]}</h3>}
          <p className="workspace-welcome-body">
            {connectionStatus === "connected"
              ? "A place to build with your models, shared context and a little more perspective."
              : connectionStatus === "connecting"
                ? "Your workspace will be ready when the connection completes."
                : "Connect using the controls above, or configure your workspace from the sidebar."}
          </p>
          {onOpenView && <div className="workspace-welcome-actions">
            <button type="button" onClick={() => onOpenView("council")}><UsersRound size={18} aria-hidden="true" /><strong>Bring a council together<ArrowUpRight size={14} aria-hidden="true" /></strong><span>Put different models around the same problem.</span></button>
            <button type="button" onClick={() => onOpenView("memory")}><BrainCircuit size={18} aria-hidden="true" /><strong>Keep the context<ArrowUpRight size={14} aria-hidden="true" /></strong><span>Explore the memory your agents work with.</span></button>
            <button type="button" onClick={() => onOpenView("workflow")}><Workflow size={18} aria-hidden="true" /><strong>See the work unfold<ArrowUpRight size={14} aria-hidden="true" /></strong><span>Follow workflows from one agent to the next.</span></button>
          </div>}
        </div>
      ) : (
        activityEntries(items, start, end).map((entry) => entry.grouped
          ? <WorkActivity key={entry.id} items={entry.items} renderItem={renderItem} />
          : renderItem(entry.items[0]))
      )}
      {working && end === items.length && (
        // The run is live and nothing is moving on screen: between
        // `RunStarted` and the first token of a cold provider that used to be
        // indistinguishable from a hang. The dot pulses (CSS, honours
        // `prefers-reduced-motion` in theme.css) and the text names the phase.
        <div style={WORKING_ROW} role="status" data-testid="run-working">
          <span aria-hidden="true" style={WORKING_DOT} />
          <span>{working}</span>
        </div>
      )}
    </div>
    </>
  );
};
