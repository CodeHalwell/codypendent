import { describe, expect, it } from "vitest";
import { initialState, reduce, type DaemonState } from "../src/daemonState.js";
import { completedPlanHandoff } from "../src/planHandoff.js";

type DurableEvent = DaemonState["durableEvents"][number];

/** Synthetic durable history; no shell, model provider, or transport is invoked. */
function event(sequence: number, body: Record<string, unknown>): DurableEvent {
  return { sequence, occurred_at: "2026-10-31T00:00:00Z", actor: { type: "System" }, body: body as never };
}

function started(sequence = 1, runId = "plan-run", mode = "Plan"): DurableEvent {
  return event(sequence, { type: "RunStarted", run_id: runId, mode: { type: mode }, objective: "Plan the implementation" });
}

function completed(sequence = 3, runId = "plan-run", disposition = "Completed"): DurableEvent {
  return event(sequence, { type: "RunCompleted", run_id: runId, disposition: { type: disposition, summary: "A completed plan" } });
}

function ready(overrides: Partial<DaemonState> = {}): DaemonState {
  return {
    ...initialState,
    status: "connected",
    activeSessionId: "session",
    sessionAttachmentConfirmed: true,
    attachingSessionId: null,
    activeRunId: null,
    isRunning: false,
    pendingGap: null,
    pendingPrompts: [],
    objectivesByRun: {},
    lastSequence: 3,
    durableEvents: [
      started(),
      event(2, { type: "ModelStreamDelta", run_id: "plan-run", text: "Review the code, then implement." }),
      completed(),
    ],
    transcript: [{ id: "assistant-2", type: "assistant", runId: "plan-run", text: "Review the code, then implement.", timestamp: "" }],
    ...overrides,
  };
}

describe("completed Plan handoff provenance", () => {
  it("attributes a completed Plan to its exact session and run", () => {
    expect(completedPlanHandoff(ready())).toEqual({ sessionId: "session", runId: "plan-run" });
  });

  it.each(["Build", "Ask", "Review", "Unknown"])("does not infer Plan from response text when its recorded mode is %s", mode => {
    expect(completedPlanHandoff(ready({ durableEvents: [started(1, "plan-run", mode), completed()] }))).toBeNull();
  });

  it("hides a completed legacy run without recorded mode provenance", () => {
    expect(completedPlanHandoff(ready({ durableEvents: [
      event(1, { type: "RunStarted", run_id: "plan-run", objective: "Plan the implementation" }),
      completed(),
    ] }))).toBeNull();
  });

  it("retires an older Plan when the newer run has no recorded mode", () => {
    const state = ready();
    state.durableEvents.push(
      event(4, { type: "RunStarted", run_id: "legacy-run", objective: "A new task" }),
      completed(5, "legacy-run"),
    );
    expect(completedPlanHandoff(state)).toBeNull();
  });

  it.each(["Failed", "Cancelled", "Unknown"])("refuses a Plan whose terminal disposition is %s", disposition => {
    expect(completedPlanHandoff(ready({ durableEvents: [started(), completed(3, "plan-run", disposition)] }))).toBeNull();
  });

  it("requires the matching RunStarted rather than a completion summary or snapshot-only text", () => {
    expect(completedPlanHandoff(ready({ durableEvents: [completed()] }))).toBeNull();
  });

  it("requires a successful RunCompleted, not just a terminal lifecycle state", () => {
    expect(completedPlanHandoff(ready({ durableEvents: [started(), event(3, { type: "RunStateChanged", run_id: "plan-run", state: { type: "Completed" } })] }))).toBeNull();
  });

  it("does not match a different run's completion", () => {
    expect(completedPlanHandoff(ready({ durableEvents: [started(), completed(3, "other-run")] }))).toBeNull();
  });

  it("does not accept a completion recorded before its start", () => {
    expect(completedPlanHandoff(ready({ durableEvents: [completed(1), started(2)] }))).toBeNull();
  });

  it.each<{ transcript: DaemonState["transcript"] }>([
    { transcript: [] },
    { transcript: [{ id: "empty", type: "assistant", runId: "plan-run", text: " \n ", timestamp: "" }] },
    { transcript: [{ id: "thought", type: "thought", runId: "plan-run", text: "This is only reasoning.", timestamp: "" }] },
    { transcript: [{ id: "foreign", type: "assistant", runId: "other-run", text: "A sibling's plan.", timestamp: "" }] },
    { transcript: [{ id: "unattributed", type: "assistant", text: "Text without a known run.", timestamp: "" }] },
  ])("requires nonempty assistant text owned by the completed run: %#", ({ transcript }) => {
    expect(completedPlanHandoff(ready({ transcript }))).toBeNull();
  });

  it.each(["Plan", "Build", "Unknown"])("retires the older Plan after a newer %s run, even if that run fails", mode => {
    const state = ready();
    state.durableEvents.push(started(4, "newer-run", mode), completed(5, "newer-run", "Failed"));
    expect(completedPlanHandoff(state)).toBeNull();
  });

  it("does not fall back to older text when the latest successful Plan has no assistant answer", () => {
    const state = ready();
    state.durableEvents.push(started(4, "newer-plan"), completed(5, "newer-plan"));
    expect(completedPlanHandoff(state)).toBeNull();
  });

  it("retires the handoff when a newer user message follows the Plan reply", () => {
    const state = ready();
    state.transcript.push({ id: "new-user", type: "user", text: "I have changed the task.", timestamp: "" });
    expect(completedPlanHandoff(state)).toBeNull();
  });

  it("blocks a still-active sibling even when the displayed Plan is finished", () => {
    const state = ready({ durableEvents: [started(1, "sibling", "Build"), started(2), completed(3)] });
    expect(completedPlanHandoff(state)).toBeNull();
  });

  it.each(["completion", "terminal state"])("permits the latest Plan once an older sibling settles by %s", settlement => {
    const state = ready({ durableEvents: [started(1, "sibling", "Build"), started(2), completed(3)] });
    state.durableEvents.push(settlement === "completion"
      ? completed(4, "sibling", "Cancelled")
      : event(4, { type: "RunStateChanged", run_id: "sibling", state: { type: "Failed" } }));
    expect(completedPlanHandoff(state)).toEqual({ sessionId: "session", runId: "plan-run" });
  });

  it.each(["assistant", "thought", "system"] as const)("checks the newest substantive answer when an older sibling leaves a later %s row", type => {
    const state = ready({
      durableEvents: [
        started(1, "older-build", "Build"),
        started(2),
        event(3, { type: "ModelStreamDelta", run_id: "plan-run", text: "Review the code, then implement." }),
        completed(4),
        event(5, type === "system"
          ? { type: "NoteAppended", text: "A later sibling note." }
          : { type: "ModelStreamDelta", run_id: "older-build", text: "A later sibling response.", thought: type === "thought" }),
        completed(6, "older-build"),
      ],
      lastSequence: 6,
    });
    state.transcript.push({ id: "later-sibling", type, runId: "older-build", text: "A later sibling response.", timestamp: "" });
    expect(completedPlanHandoff(state)).toEqual(type === "assistant" ? null : { sessionId: "session", runId: "plan-run" });
  });
});

describe("completed Plan handoff availability", () => {
  it.each<Partial<DaemonState>>([
    { status: "disconnected" },
    { status: "connecting" },
    { activeSessionId: null },
    { sessionAttachmentConfirmed: false },
    { attachingSessionId: "other-session" },
    { isRunning: true },
    { activeRunId: "still-active" },
    { pendingGap: { sessionId: "session", after: 1, through: 2 } },
    { pendingPrompts: [{ id: "queued-prompt" } as never] },
    { promptQueueError: "The queue state could not be confirmed." },
  ])("hides during an uncertain or busy session boundary: %#", overrides => {
    expect(completedPlanHandoff(ready(overrides))).toBeNull();
  });

  it.each(["approval", "question"] as const)("hides while an unresolved %s card has no status field", type => {
    const state = ready();
    state.transcript.push({ id: "decision", type, text: "A decision is pending.", timestamp: "", approvalId: type === "approval" ? "approval" : undefined, questionId: type === "question" ? "question" : undefined });
    expect(completedPlanHandoff(state)).toBeNull();
  });

  it("keeps the same attribution after duplicate durable replay", () => {
    const events = ready().durableEvents;
    const fold = (state: DaemonState, entry: DurableEvent) => reduce(state, { type: "frame", frame: { kind: "event", session_id: "session", event: entry } });
    const attached = ready({ durableEvents: [], transcript: [], lastSequence: 0 });
    const settled = events.reduce(fold, attached);
    const replayed = events.reduce(fold, settled);
    expect({ candidate: completedPlanHandoff(replayed), events: replayed.durableEvents.length, answers: replayed.transcript.filter(item => item.type === "assistant").length }).toEqual({
      candidate: { sessionId: "session", runId: "plan-run" }, events: 3, answers: 1,
    });
  });
});
