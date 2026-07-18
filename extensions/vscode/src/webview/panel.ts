/**
 * Webview transcript panel — pure HTML/JS generation (no `vscode` import, so it
 * is importable in tests). `extension.ts` supplies a nonce and the webview's
 * `cspSource` and posts `TranscriptMessage`s in; the panel renders the session
 * transcript and current run state, and posts approval decisions back out.
 *
 * The panel keeps only VIEW state (a rolling transcript in the DOM). Session
 * truth lives in the daemon's ledger and is recovered on reload via
 * attach-resume, so nothing here is authoritative.
 */
import { randomBytes } from "node:crypto";

/** Messages posted from the extension host into the webview. */
export type TranscriptMessage =
  | { kind: "status"; status: string }
  | { kind: "event"; sequence: number; label: string; detail: string }
  | { kind: "runState"; runId: string; state: string }
  | { kind: "approval"; approvalId: string; summary: string; risk: string }
  | { kind: "approvalResolved"; approvalId: string; decision: string }
  | { kind: "clear" };

/** Messages posted from the webview back to the extension host. */
export type WebviewCommandMessage =
  | { kind: "approve"; approvalId: string }
  | { kind: "reject"; approvalId: string }
  | { kind: "startRun"; objective: string };

export interface PanelHtmlOptions {
  nonce: string;
  cspSource: string;
}

/**
 * Build the full webview HTML. A strict CSP allows only the nonce'd inline
 * script and styles from `cspSource`; there are no external resources.
 */
export function renderPanelHtml(options: PanelHtmlOptions): string {
  const { nonce, cspSource } = options;
  const csp = [
    "default-src 'none'",
    `style-src ${cspSource} 'unsafe-inline'`,
    `script-src 'nonce-${nonce}'`,
  ].join("; ");

  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8" />
<meta http-equiv="Content-Security-Policy" content="${csp}" />
<meta name="viewport" content="width=device-width, initial-scale=1.0" />
<title>Codypendent Session</title>
<style>
  :root { color-scheme: light dark; }
  body {
    font-family: var(--vscode-font-family, sans-serif);
    font-size: var(--vscode-font-size, 13px);
    color: var(--vscode-foreground);
    padding: 0.5rem;
    margin: 0;
  }
  header { display: flex; align-items: center; gap: 0.5rem; margin-bottom: 0.5rem; }
  .status { font-size: 11px; padding: 2px 6px; border-radius: 3px;
    background: var(--vscode-badge-background); color: var(--vscode-badge-foreground); }
  .run-state { font-size: 11px; opacity: 0.8; }
  #transcript { display: flex; flex-direction: column; gap: 4px; }
  .entry { padding: 4px 6px; border-left: 2px solid var(--vscode-panel-border);
    white-space: pre-wrap; word-break: break-word; }
  .entry .label { font-weight: 600; }
  .entry .seq { opacity: 0.5; font-size: 10px; margin-right: 6px; }
  .approval { border: 1px solid var(--vscode-inputValidation-warningBorder, #b89500);
    border-radius: 4px; padding: 6px; margin: 4px 0; }
  .approval .risk { font-size: 11px; opacity: 0.8; }
  .approval .actions { display: flex; gap: 6px; margin-top: 6px; }
  button {
    font-family: inherit; font-size: 12px; border: none; border-radius: 3px;
    padding: 3px 10px; cursor: pointer;
    color: var(--vscode-button-foreground); background: var(--vscode-button-background);
  }
  button.reject { background: var(--vscode-button-secondaryBackground);
    color: var(--vscode-button-secondaryForeground); }
  button:hover { background: var(--vscode-button-hoverBackground); }
  .resolved { opacity: 0.6; font-size: 11px; }
</style>
</head>
<body>
<header>
  <span class="status" id="status">closed</span>
  <span class="run-state" id="run-state"></span>
</header>
<div id="approvals"></div>
<div id="transcript"></div>
<script nonce="${nonce}">
  const vscode = acquireVsCodeApi();
  const statusEl = document.getElementById('status');
  const runStateEl = document.getElementById('run-state');
  const transcriptEl = document.getElementById('transcript');
  const approvalsEl = document.getElementById('approvals');
  const approvalNodes = new Map();

  const MAX_ENTRIES = 500;
  const scroller = document.scrollingElement || document.documentElement;
  function nearBottom() {
    return (scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight) < 40;
  }

  function addEntry(sequence, label, detail) {
    const stick = nearBottom();
    const entry = document.createElement('div');
    entry.className = 'entry';
    const seq = document.createElement('span');
    seq.className = 'seq';
    seq.textContent = sequence != null ? ('#' + sequence) : '';
    const lab = document.createElement('span');
    lab.className = 'label';
    lab.textContent = label + ' ';
    const det = document.createElement('span');
    det.textContent = detail || '';
    entry.appendChild(seq);
    entry.appendChild(lab);
    entry.appendChild(det);
    transcriptEl.appendChild(entry);
    // Cap the DOM so an hours-long streaming session does not grow unbounded.
    while (transcriptEl.childElementCount > MAX_ENTRIES) {
      transcriptEl.removeChild(transcriptEl.firstChild);
    }
    // Only autoscroll if the user was already at the bottom — don't yank the
    // view down while they are scrolled up reading history.
    if (stick) {
      entry.scrollIntoView({ block: 'end' });
    }
  }

  function addApproval(approvalId, summary, risk) {
    if (approvalNodes.has(approvalId)) return;
    const card = document.createElement('div');
    card.className = 'approval';
    const title = document.createElement('div');
    title.textContent = summary;
    const riskEl = document.createElement('div');
    riskEl.className = 'risk';
    riskEl.textContent = 'risk: ' + risk;
    const actions = document.createElement('div');
    actions.className = 'actions';
    const approve = document.createElement('button');
    approve.textContent = 'Approve';
    approve.onclick = () => vscode.postMessage({ kind: 'approve', approvalId });
    const reject = document.createElement('button');
    reject.className = 'reject';
    reject.textContent = 'Reject';
    reject.onclick = () => vscode.postMessage({ kind: 'reject', approvalId });
    actions.appendChild(approve);
    actions.appendChild(reject);
    card.appendChild(title);
    card.appendChild(riskEl);
    card.appendChild(actions);
    approvalsEl.appendChild(card);
    approvalNodes.set(approvalId, card);
  }

  function resolveApproval(approvalId, decision) {
    const card = approvalNodes.get(approvalId);
    if (!card) return;
    card.innerHTML = '';
    card.className = 'resolved';
    card.textContent = 'Approval ' + approvalId.slice(0, 8) + ' -> ' + decision;
  }

  window.addEventListener('message', (event) => {
    const msg = event.data;
    switch (msg.kind) {
      case 'status': statusEl.textContent = msg.status; break;
      case 'runState': runStateEl.textContent = 'run ' + msg.runId.slice(0, 8) + ': ' + msg.state; break;
      case 'event': addEntry(msg.sequence, msg.label, msg.detail); break;
      case 'approval': addApproval(msg.approvalId, msg.summary, msg.risk); break;
      case 'approvalResolved': resolveApproval(msg.approvalId, msg.decision); break;
      case 'clear':
        transcriptEl.innerHTML = '';
        approvalsEl.innerHTML = '';
        approvalNodes.clear();
        break;
    }
  });
</script>
</body>
</html>`;
}

/** A cryptographically strong nonce for the webview CSP. */
export function makeNonce(): string {
  return randomBytes(16).toString("hex");
}
