import type { DaemonState } from "./daemonState.js";

/** Display eligibility comes from the session ledger, never the staged next-run mode. */
export function completedPlanHandoff(state: DaemonState): { runId: string; sessionId: string } | null {
  if (state.status !== "connected" || !state.sessionAttachmentConfirmed || state.attachingSessionId !== null ||
      !state.activeSessionId || state.isRunning || state.activeRunId !== null || state.pendingGap ||
      state.pendingPrompts.length || state.promptQueueError ||
      state.transcript.some((item) => item.type === "approval" || item.type === "question")) return null;

  const live = new Set<string>();
  let latest: { runId: string; mode: string; sequence: number } | null = null;
  const completions = new Map<string, { outcome: string; sequence: number }>();
  for (const event of state.durableEvents) {
    const body = event.body;
    if (body.type === "RunStarted") {
      latest = { runId: body.run_id, mode: body.mode?.type ?? "Unknown", sequence: event.sequence };
      live.add(body.run_id);
    } else if (body.type === "RunStateChanged") {
      if (["Completed", "Failed", "Cancelled"].includes(body.state.type)) live.delete(body.run_id);
      else live.add(body.run_id);
      if (body.state.type === "Failed" || body.state.type === "Cancelled") completions.delete(body.run_id);
    } else if (body.type === "RunCompleted") {
      live.delete(body.run_id);
      completions.set(body.run_id, { outcome: body.disposition.type, sequence: event.sequence });
    }
  }
  if (live.size || latest?.mode !== "Plan") return null;
  const completion = completions.get(latest.runId);
  if (completion?.outcome !== "Completed" || completion.sequence <= latest.sequence) return null;
  let replyIndex = -1;
  for (let index = state.transcript.length - 1; index >= 0; index--) {
    const item = state.transcript[index];
    if (item.type === "assistant" && item.text.trim()) {
      if (item.runId !== latest.runId) return null;
      replyIndex = index;
      break;
    }
  }
  if (replyIndex < 0 || state.transcript.slice(replyIndex + 1).some((item) => item.type === "user")) return null;
  return { runId: latest.runId, sessionId: state.activeSessionId };
}
