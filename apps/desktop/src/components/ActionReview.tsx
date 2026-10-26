import type { ReactNode } from "react";
import type { ApprovalAction } from "../approval.js";
import { desktopToolName, jsonData, toolTitle } from "./toolPresentation.js";

type Field = [label: string, value: ReactNode];
const lines = (values: string[]) => Array.isArray(values) ? <>
  {values.slice(0, 4).map((value, index) => <div key={index}>{approvalText(value) ?? "See technical details"}</div>)}
  {values.length > 4 && <div>{values.length - 4} more in technical details</div>}
</> : null;

/** Approval descriptions are UI copy; payloads and source belong in the disclosure. */
export function approvalText(value: unknown): string | undefined {
  if (typeof value !== "string") return undefined;
  const text = value.trim();
  if (!text || text.length > 280 || jsonData(text) !== undefined
    || /```|[\[{]\s*["'{\[]|["']?(?:functionBody|generatedTool|inputSchema|source)["']?\s*:/.test(text)
    || /^(?:async\s+)?function\b|^(?:return|import|export|const|let|var)\s|=>|<(?:script|html|body|div|button|style)\b/.test(text)
    || /^(?:McpToolCall|AcpToolCall)(?:$|\s*:|\s*\{)/.test(text)) return undefined;
  return text;
}

/** Keep the consequence visible; internal approval-routing explanations stay in details. */
export function approvalRiskText(value: unknown): string | undefined {
  const text = approvalText(value);
  if (!text || /\b(?:MCP|ACP|JSON[- ]?RPC|transport|protocol|tool[_-]call)\b|\b(?:policy|rule|reason)[ _-]?(?:code|id)\s*[:=]|\b[a-z]+_[a-z_]+\b/i.test(text)) return undefined;
  return text;
}

function object(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : undefined;
}

function argumentsObject(value: unknown): Record<string, unknown> {
  if (typeof value !== "string") return object(value) ?? {};
  try { return object(JSON.parse(value)) ?? {}; } catch { return {}; }
}

// Some ACP clients use a serialized native action as their permission title.
// This affects presentation only: canReviewApproval still gates the original action.
function embeddedMcpAction(value: string): Extract<ApprovalAction, { type: "McpToolCall" }> | undefined {
  let index = -1;
  for (let attempt = 0; attempt < 8; attempt++) {
    index = value.indexOf("{", index + 1);
    if (index < 0) return undefined;
    try {
      const parsed = object(JSON.parse(value.slice(index)));
      const row = object(parsed?.action) ?? parsed;
      if (row?.type === "McpToolCall" && typeof row.server === "string" && typeof row.tool === "string") {
        return { type: "McpToolCall", server: row.server, tool: row.tool,
          summary: typeof row.summary === "string" ? row.summary : "",
          args: typeof row.args === "string" ? row.args : JSON.stringify(row.args ?? {}) };
      }
    } catch { /* Preserve unrecognised wrappers in the exact details below. */ }
  }
  return undefined;
}

const detailText = (value: unknown) => approvalText(value) ?? "See technical details";

function actionPresentation(action: ApprovalAction): { summary: string; fields: Field[] } {
  switch (action.type) {
    case "ReadFiles": return { summary: "Read project files", fields: [["Files", lines(action.paths)]] };
    case "WritePatch": return { summary: "Apply proposed file changes", fields: [] };
    case "ExecuteCommand": return { summary: "Run a command", fields: [
      ["Command", <code>{detailText(typeof action.program === "string" ? [action.program, ...(Array.isArray(action.args) ? action.args : [])].join(" ") : (action as unknown as Record<string, unknown>).command)}</code>],
      ...(action.cwd ? [["Working directory", action.cwd] as Field] : []),
      ...(Array.isArray(action.environment) ? [["Environment", action.environment.length > 0
        ? lines(action.environment.map(([name, value]) => detailText(`${name}=${value}`)))
        : "No environment variables"] as Field] : []),
    ] };
    case "NetworkRequest": return { summary: "Connect to a network destination", fields: [["Destination", action.destination]] };
    case "GitCommit": return { summary: "Commit repository changes", fields: [["Repository", action.repository]] };
    case "GitPush": return { summary: "Push repository changes", fields: [["Remote", action.remote], ["Branch", action.branch]] };
    case "GitHubMutation": return { summary: approvalText(action.summary) ?? "Update GitHub", fields: [["Repository", action.repository]] };
    case "PublishDocument": return { summary: "Publish a document", fields: [["Destination", action.target], ["Action", action.git_action], ["Files", lines(action.changed_files)]] };
    case "McpToolCall": {
      if (action.server !== "desktop" && action.server !== "codypendent-desktop") return {
        summary: approvalText(action.summary) ?? "Use a connected tool",
        fields: [["Connection", detailText(action.server)], ["Tool", detailText(action.tool)]],
      };
      const args = argumentsObject(action.args);
      const generated = object(args.generatedTool) ?? object(args.definition) ?? args;
      const capabilities = Array.isArray(generated.capabilities) ? generated.capabilities.filter((name): name is string => typeof name === "string") : [];
      return { summary: desktopToolName(action.tool) ? toolTitle(action.tool) : "Use a workspace tool", fields: [
        ...(approvalText(generated.name) ? [["Helper", toolTitle(generated.name as string)] as Field] : []),
        ...(approvalText(generated.description) ? [["Purpose", generated.description as string] as Field] : []),
        ...(Array.isArray(generated.capabilities) ? [["Workspace access", capabilities.length ? lines(capabilities.map((name) => detailText(toolTitle(name)))) : "No workspace capabilities"] as Field] : []),
        ...["view", "appearance", "path", "line", "query"].flatMap((name): Field[] =>
          approvalText(args[name]) || typeof args[name] === "number"
            ? [[name === "view" ? "Page" : name[0].toUpperCase() + name.slice(1), String(args[name])]] : []),
      ] };
    }
    case "AcpToolCall": {
      const nested = embeddedMcpAction(action.title) ?? embeddedMcpAction(action.details);
      const presentation = nested ? actionPresentation(nested) : {
        summary: desktopToolName(action.title) ? toolTitle(action.title)
          : approvalText(action.title) && !/^(?:mcp[._]|cpd_)/.test(action.title) ? action.title : "Use an agent tool",
        fields: [] as Field[],
      };
      return { ...presentation, fields: [...presentation.fields, ["Agent", detailText(action.agent)]] };
    }
    case "CouncilCreate": return { summary: "Create a council", fields: [["Council", action.name], ["Purpose", detailText(action.summary)]] };
    case "CouncilRun": return { summary: "Run a council", fields: [["Council", action.name], ["Task", detailText(action.summary)]] };
    case "WorkflowCreate": return { summary: "Create a workflow", fields: [["Workflow", action.workflow_id], ["Purpose", detailText(action.summary)]] };
    case "WorkflowRun": return { summary: "Run a workflow", fields: [["Workflow", action.workflow_id], ["Task", detailText(action.summary)]] };
    case "RestoreCheckpoint": return { summary: "Restore a checkpoint", fields: [["Checkpoint", `${action.ordinal} · ${action.commit}`], ["Working directory", action.worktree]] };
    case "WriteProcessStdin": return { summary: "Send input to a running process", fields: [["Process", action.process_id], ["Input size", `${action.byte_len} bytes`]] };
    default: return { summary: "Review the proposed action", fields: [] };
  }
}

export function ActionReview({ action, fallback }: { action?: ApprovalAction; fallback: string }) {
  const presentedAction = action ?? embeddedMcpAction(fallback);
  const { summary, fields } = presentedAction ? actionPresentation(presentedAction)
    : { summary: approvalText(fallback) ?? "Review the proposed action", fields: [] };
  return <div className="transcript-action-review">
    <p>{summary}</p>
    {fields.length > 0 && <dl>{fields.map(([label, value]) => <div key={label}><dt>{label}</dt><dd>{value}</dd></div>)}</dl>}
    <details className="transcript-technical-details">
      <summary>Technical details</summary>
      <pre aria-label="Proposed action details" tabIndex={0}>{action ? JSON.stringify(action, null, 2) : fallback}</pre>
    </details>
  </div>;
}
