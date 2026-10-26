import type { ApprovalScope, EventBody } from "@codypendent/protocol";

export type ApprovalAction = Extract<EventBody, { type: "ApprovalRequested" }>["action"];
export type ApprovalGrant = Exclude<ApprovalScope["type"], "Unknown">;

export function reusableApprovalPattern(action: ApprovalAction | undefined, pattern?: string): string | undefined {
  return action?.type === "ExecuteCommand" && pattern?.trim() ? pattern : undefined;
}

// Approval is available only for action kinds this client can explain. Keep
// internal reads out of this list: they do not normally reach the policy gate.
const reviewable = new Set<ApprovalAction["type"]>([
  "ReadFiles", "WritePatch", "ExecuteCommand", "NetworkRequest", "GitCommit", "GitPush",
  "GitHubMutation", "PublishDocument", "McpToolCall", "AcpToolCall", "CouncilCreate",
  "CouncilRun", "WorkflowCreate", "WorkflowRun", "RestoreCheckpoint", "WriteProcessStdin",
]);

export function canReviewApproval(action: ApprovalAction | undefined): boolean {
  return action !== undefined && reviewable.has(action.type);
}
