import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { waitFor } from "@testing-library/react";
import { AbstractAgent } from "@ag-ui/client";
import { EventType, type BaseEvent } from "@ag-ui/core";
import type { EventBody, SessionEvent } from "@codypendent/protocol";
import { NativeAgent } from "../src/agui/NativeAgent.js";
import { publishFrame, resetFrameBus } from "../src/frameBus.js";

let sequence = 0;
const agents: NativeAgent[] = [];
function event(body: EventBody, sessionId = "session-1", at = ++sequence) {
  const value: SessionEvent = { body, sequence: at, occurred_at: "2026-10-19T09:00:00Z", actor: { type: "System" } };
  publishFrame({ kind: "event", session_id: sessionId, event: value });
  return value;
}
function started(objective = "Do the work", runId = "native-run", sessionId = "session-1") {
  event({ type: "RunStarted", run_id: runId, objective, mode: { type: "Build" } }, sessionId);
}
function agentWith(submit: (objective: string) => Promise<boolean> = async () => true, sessionId: () => string | null = () => "session-1") {
  const agent = new NativeAgent({ submit, sessionId, threadId: "agui-thread" });
  agent.observeNative();
  const events: BaseEvent[] = [];
  agent.subscribe({ onEvent: ({ event }) => { events.push(event); } });
  agents.push(agent);
  return { agent, events };
}
beforeEach(() => { sequence = 0; resetFrameBus(); });
afterEach(() => { agents.splice(0).forEach((agent) => agent.dispose()); resetFrameBus(); });

it("is a standard AbstractAgent and resolves acceptance before native completion", async () => {
  const submit = vi.fn(async () => true);
  const { agent, events } = agentWith(submit);
  expect(agent).toBeInstanceOf(AbstractAgent);
  expect(await agent.submitObjective("Do the work")).toBe(true);
  expect(agent.isRunning).toBe(true);
  expect(submit).toHaveBeenCalledExactlyOnceWith("Do the work");
  started();
  event({ type: "ModelStreamDelta", run_id: "native-run", text: "A useful " });
  event({ type: "ModelStreamDelta", run_id: "native-run", text: "answer" });
  event({ type: "RunStateChanged", run_id: "native-run", state: { type: "Completed" } });
  await waitFor(() => expect(agent.isRunning).toBe(false));
  const start = events.find((event) => event.type === EventType.RUN_STARTED)!;
  const end = events.find((event) => event.type === EventType.RUN_FINISHED)!;
  expect(start.threadId).toBe("agui-thread");
  expect(start.runId).not.toBe("native-run");
  expect(end.runId).toBe(start.runId);
  expect(start.metadata?.codypendent).toMatchObject({ sessionId: "session-1", runId: "native-run" });
  expect(agent.messages.find((message) => message.role === "assistant")?.content).toBe("A useful answer");
  expect(events.filter((event) => event.type === EventType.TEXT_MESSAGE_END)).toHaveLength(1);
});

it("adapts events that arrive synchronously before the native submit reply", async () => {
  let accept!: (accepted: boolean) => void;
  const { agent, events } = agentWith(async (objective) => {
    started(objective);
    event({ type: "ModelStreamDelta", run_id: "native-run", text: "Already working" });
    return new Promise((resolve) => { accept = resolve; });
  });
  let accepted = false;
  const submitted = agent.submitObjective("Do the work").then((value) => { accepted = value; return value; });
  await waitFor(() => expect(events.some((item) => item.type === EventType.TEXT_MESSAGE_CONTENT)).toBe(true));
  expect(accepted).toBe(false);
  accept(true);
  expect(await submitted).toBe(true);
  event({ type: "RunStateChanged", run_id: "native-run", state: { type: "Completed" } });
  await waitFor(() => expect(agent.isRunning).toBe(false));
});

it("supports standard runAgent callers and finite synchronous native runs", async () => {
  const { agent, events } = agentWith(async (objective) => {
    started(objective);
    event({ type: "RunCompleted", run_id: "native-run", disposition: { type: "Completed" }, chronicle: artifact });
    return true;
  });
  agent.addMessage({ id: "user-1", role: "user", content: "Do the work" });
  await agent.runAgent({ runId: "agui-run" });
  expect(events.find((item) => item.type === EventType.RUN_STARTED)).toMatchObject({ runId: "agui-run", threadId: "agui-thread" });
  expect(events.filter((item) => item.type === EventType.RUN_FINISHED)).toHaveLength(1);
});

it("rejects duplicate submissions and simultaneous standard runs", async () => {
  const submit = vi.fn(async () => true);
  const { agent } = agentWith(submit);
  const first = agent.submitObjective("Do the work");
  expect(await agent.submitObjective("A duplicate")).toBe(false);
  await expect(agent.runAgent()).rejects.toThrow("already active");
  expect(await first).toBe(true);
  expect(submit).toHaveBeenCalledTimes(1);
});

it("ignores history, catchup, old sequences and runs, foreign sessions and sibling runs", async () => {
  const { agent, events } = agentWith();
  const old = event({ type: "RunStarted", run_id: "old-run", objective: "Do the work", mode: { type: "Build" } });
  expect(await agent.submitObjective("Do the work")).toBe(true);
  publishFrame({ kind: "history", session_id: "session-1", through: 100, events: [old] });
  publishFrame({ kind: "catchup", session_id: "session-1", snapshot: {} as never });
  started("Do the work", "foreign-run", "different-session");
  started("Do the work", "old-run");
  event({ type: "RunStarted", run_id: "stale-run", objective: "Do the work", mode: { type: "Build" } }, "session-1", old.sequence);
  started();
  event({ type: "ModelStreamDelta", run_id: "sibling-run", text: "WRONG" });
  const delta = event({ type: "ModelStreamDelta", run_id: "native-run", text: "RIGHT" });
  publishFrame({ kind: "event", session_id: "session-1", event: delta });
  event({ type: "RunStateChanged", run_id: "sibling-run", state: { type: "Completed" } });
  expect(agent.isRunning).toBe(true);
  event({ type: "RunStateChanged", run_id: "native-run", state: { type: "Completed" } });
  await waitFor(() => expect(agent.isRunning).toBe(false));
  expect(events.filter((item) => item.type === EventType.RUN_STARTED)).toHaveLength(1);
  expect(events.filter((item) => item.type === EventType.TEXT_MESSAGE_CONTENT).map((item) => item.delta)).toEqual(["RIGHT"]);
});

it("keeps tool invocation identity, never reconstructs arguments, and reports actual outcomes", async () => {
  const { agent, events } = agentWith();
  await agent.submitObjective("Do the work");
  started();
  for (const name of ["acp:call-a", "acp:call-b"]) event({ type: "ToolStarted", run_id: "native-run", tool: name, args_digest: "digest-only", label: "Read source" });
  event({ type: "ToolStarted", run_id: "native-run", tool: "acp:call-a", args_digest: "digest-only", label: "Read refined source" });
  event({ type: "ToolUpdated", run_id: "native-run", tool: "acp:call-a", label: "Read src/main.rs" });
  event({ type: "ToolCompleted", run_id: "native-run", tool: "acp:call-b", outcome: { type: "Failed", message: "Missing file" } });
  event({ type: "ToolCompleted", run_id: "native-run", tool: "acp:call-a", outcome: { type: "Succeeded" }, artifact });
  event({ type: "RunStateChanged", run_id: "native-run", state: { type: "Completed" } });
  await waitFor(() => expect(agent.isRunning).toBe(false));
  const starts = events.filter((item) => item.type === EventType.TOOL_CALL_START);
  const results = events.filter((item) => item.type === EventType.TOOL_CALL_RESULT);
  expect(starts).toHaveLength(2);
  expect(events.some((item) => item.type === EventType.TOOL_CALL_ARGS)).toBe(false);
  expect(starts[0].metadata?.codypendent).toMatchObject({ argsDigest: "digest-only" });
  expect(results.map((item) => item.toolCallId)).toEqual([starts[1].toolCallId, starts[0].toolCallId]);
  expect(JSON.parse(results[0].content as string)).toEqual({ outcome: { type: "Failed", message: "Missing file" } });
  expect(JSON.parse(results[1].content as string)).toEqual({ outcome: { type: "Succeeded" }, artifact });
});

it("preserves approvals and questions as linked custom events without resolving them", async () => {
  const { agent, events } = agentWith();
  await agent.submitObjective("Do the work");
  started();
  const action = { type: "ReadFiles" as const, paths: ["src/main.rs"] };
  event({ type: "ToolProposed", run_id: "native-run", approval_id: "approval-1", action });
  event({ type: "ApprovalRequested", approval_id: "approval-1", action, risk: { level: { type: "Low" } } });
  event({ type: "ApprovalRequested", approval_id: "foreign-approval", action, risk: { level: { type: "Low" } } });
  event({ type: "QuestionAsked", run_id: "native-run", question_id: "question-1", questions: [] });
  event({ type: "QuestionResolved", question_id: "foreign-question", outcome: { type: "Rejected" } });
  await waitFor(() => expect(events.filter((item) => item.type === EventType.CUSTOM && /ApprovalRequested|QuestionAsked/.test(String(item.name)))).toHaveLength(2));
  expect(agent.isRunning).toBe(true);
  expect(events.some((item) => item.type === EventType.RUN_FINISHED)).toBe(false);
});

it.each(["Completed", "Failed", "Cancelled"] as const)("finishes terminal RunStateChanged.%s and closes open text/reasoning", async (state) => {
  const { agent, events } = agentWith();
  await agent.submitObjective("Do the work"); started();
  event({ type: "ModelStreamDelta", run_id: "native-run", text: "Thinking", thought: true });
  event({ type: "ModelStreamDelta", run_id: "native-run", text: "Answer" });
  event({ type: "RunStateChanged", run_id: "native-run", state: { type: state } });
  await waitFor(() => expect(agent.isRunning).toBe(false));
  expect(events.filter((item) => item.type === EventType.REASONING_MESSAGE_END)).toHaveLength(1);
  expect(events.filter((item) => item.type === EventType.TEXT_MESSAGE_END)).toHaveLength(1);
  expect(events.at(-1)?.type).toBe(state === "Failed" ? EventType.RUN_ERROR : EventType.RUN_FINISHED);
  if (state === "Cancelled") expect(events.at(-1)?.outcome).toEqual({ type: "cancelled" });
});

it("preserves native failure reasons, rejects failed submits, and ends on disconnect", async () => {
  const refused = agentWith(async () => false);
  expect(await refused.agent.submitObjective("Do the work")).toBe(false);
  await waitFor(() => expect(refused.events.at(-1)?.type).toBe(EventType.RUN_ERROR));
  const rejected = agentWith(async () => { throw new Error("Provider unavailable"); });
  expect(await rejected.agent.submitObjective("Do the work")).toBe(false);
  await waitFor(() => expect(rejected.events.at(-1)?.message).toBe("Provider unavailable"));
  const failed = agentWith();
  await failed.agent.submitObjective("Do the work"); started();
  event({ type: "RunCompleted", run_id: "native-run", disposition: { type: "Failed", reason: "Native failure detail" }, chronicle: artifact });
  await waitFor(() => expect(failed.events.at(-1)?.message).toBe("Native failure detail"));
  const disconnected = agentWith();
  await disconnected.agent.submitObjective("Do the work"); started("Do the work", "new-run");
  publishFrame({ kind: "disconnected", reason: "Workspace disconnected" });
  await waitFor(() => expect(disconnected.events.at(-1)?.message).toBe("Workspace disconnected"));
});

it("disposes subscriptions and settles a pending acceptance without submitting again", async () => {
  let accept!: (accepted: boolean) => void;
  const submit = vi.fn(() => new Promise<boolean>((resolve) => { accept = resolve; }));
  const { agent, events } = agentWith(submit);
  const submitted = agent.submitObjective("Do the work");
  await waitFor(() => expect(submit).toHaveBeenCalledTimes(1));
  agent.dispose();
  expect(await submitted).toBe(false);
  accept(true);
  await waitFor(() => expect(agent.isRunning).toBe(false));
  const before = events.length;
  started();
  event({ type: "ModelStreamDelta", run_id: "native-run", text: "After disposal" });
  await Promise.resolve();
  expect(events).toHaveLength(before);
  const next = agent.submitObjective("Another objective");
  await waitFor(() => expect(submit).toHaveBeenCalledTimes(2));
  accept(true);
  expect(await next).toBe(true);
});

const artifact = { id: "artifact-1", sha256: "a".repeat(64), byte_length: 20, media_type: "text/plain", sensitivity: { type: "Public" as const } };

it("can be reused after StrictMode cleanup without duplicate subscriptions", async () => {
  const submit = vi.fn(async () => true);
  const { agent, events } = agentWith(submit);
  agent.close();
  agent.close();
  expect(await agent.submitObjective("Do the work")).toBe(true);
  started();
  event({ type: "ModelStreamDelta", run_id: "native-run", text: "One copy" });
  event({ type: "RunStateChanged", run_id: "native-run", state: { type: "Completed" } });
  await waitFor(() => expect(agent.isRunning).toBe(false));
  expect(events.filter((item) => item.type === EventType.TEXT_MESSAGE_CONTENT)).toHaveLength(1);
  expect(submit).toHaveBeenCalledTimes(1);
});

it("does not submit an asynchronously initialised run after cleanup", async () => {
  const submit = vi.fn(async () => true);
  const { agent } = agentWith(submit);
  const submitted = agent.submitObjective("Do the work");
  agent.close();
  expect(await submitted).toBe(false);
  await waitFor(() => expect(agent.isRunning).toBe(false));
  expect(submit).not.toHaveBeenCalled();
  expect(await agent.submitObjective("A fresh objective")).toBe(true);
  expect(submit).toHaveBeenCalledExactlyOnceWith("A fresh objective");
});
