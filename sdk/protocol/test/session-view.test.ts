import { describe, expect, it } from "vitest";
import { projectSession } from "../src/session-view.js";
import type { EventBody, SessionEvent } from "../src/index.js";

function events(...bodies: EventBody[]): SessionEvent[] {
  return bodies.map((body, index) => ({ body, sequence: index + 1, occurred_at: "2026-10-26T00:00:00Z", actor: { type: "System" } }));
}

describe("shared activity timeline", () => {
  it("preserves the recorded run mode through incremental completion and history replay", () => {
    const recorded = events(
      { type: "RunStarted", run_id: "plan", objective: "Plan the change", mode: { type: "Plan" } },
      { type: "RunStateChanged", run_id: "plan", state: { type: "Running" } },
      { type: "RunCompleted", run_id: "plan", disposition: { type: "Completed", summary: "The plan" },
        chronicle: { id: "chronicle", media_type: "text/plain", byte_length: 0, sha256: "0".repeat(64), sensitivity: { type: "Internal" } } },
      { type: "RunUsage", run_id: "plan", prompt_tokens: 12, completion_tokens: 4 },
    );
    const started = projectSession({ ready: true, cursor: 1, droppedEvents: 0, events: recorded.slice(0, 1) });
    const completed = projectSession({ ready: true, cursor: 3, droppedEvents: 0, events: recorded.slice(1, 3) }, started);
    const live = projectSession({ ready: true, cursor: 4, droppedEvents: 0, events: recorded.slice(3) }, completed);
    const history = projectSession({ ready: true, cursor: 4, droppedEvents: 0, events: recorded });
    expect(live.runs).toEqual(history.runs);
    expect(history.runs).toEqual([{ id: "plan", objective: "Plan the change", mode: "Plan", state: "Completed",
      disposition: { type: "Completed", summary: "The plan" }, usage: { inputTokens: 12, outputTokens: 4, costMicros: undefined } }]);
  });

  it("leaves snapshot-only run modes unknown until their history supplies RunStarted", () => {
    const snapshot = { ready: true, cursor: 3, droppedEvents: 0, events: [], projection: {
      session_id: "session", title: "Restored", last_sequence: 3, active_runs: ["plan"], closed: false,
    } };
    const baseline = projectSession(snapshot);
    expect(baseline.runs).toEqual([{ id: "plan", state: "Running" }]);
    const restored = projectSession({ ...snapshot, events: events(
      { type: "RunStarted", run_id: "plan", objective: "Plan the change", mode: { type: "Plan" } },
      { type: "RunStateChanged", run_id: "plan", state: { type: "Running" } },
      { type: "RunStateChanged", run_id: "plan", state: { type: "Paused" } },
    ) }, baseline);
    const next = projectSession({ ...snapshot, cursor: 4, projection: undefined, events: events(
      { type: "RunStateChanged", run_id: "plan", state: { type: "Completed" } },
    ).map(event => ({ ...event, sequence: 4 })) }, restored);
    expect(next.runs).toEqual([{ id: "plan", state: "Completed", objective: "Plan the change", mode: "Plan" }]);
  });

  it("retains daemon-recorded models across live updates, model changes and history replay", () => {
    const recorded = events(
      { type: "ModelStreamDelta", run_id: "a", text: "First " },
      { type: "ModelStreamDelta", run_id: "a", text: "answer" },
      { type: "ModelStreamDelta", run_id: "a", text: "Switched " },
      { type: "ModelStreamDelta", run_id: "a", text: "answer" },
      { type: "ModelStreamDelta", run_id: "b", text: "Sibling answer" },
    ).map((event, index): SessionEvent => ({ ...event, actor: {
      type: "Agent", agent_id: "agent", run_id: index === 4 ? "b" : "a",
      model: index === 2 || index === 3 ? "acp/claude-acp#opus" : "openai/gpt-6.1-sol",
    } }));
    const initial = projectSession({ ready: true, cursor: 2, droppedEvents: 0, events: recorded.slice(0, 2) });
    const live = projectSession({ ready: true, cursor: 5, droppedEvents: 0, events: recorded.slice(2) }, initial);
    const replay = projectSession({ ready: true, cursor: 5, droppedEvents: 0, events: recorded });
    expect(live.rows).toEqual(replay.rows);
    expect(live.rows.map(({ text, runId, modelId }) => ({ text, runId, modelId }))).toEqual([
      { text: "First answer", runId: "a", modelId: "openai/gpt-6.1-sol" },
      { text: "Switched answer", runId: "a", modelId: "acp/claude-acp#opus" },
      { text: "Sibling answer", runId: "b", modelId: "openai/gpt-6.1-sol" },
    ]);
  });

  it("leaves missing, foreign and empty model attribution absent without borrowing a previous model", () => {
    const recorded = events(
      { type: "ModelStreamDelta", run_id: "a", text: "Recorded" },
      { type: "ModelStreamDelta", run_id: "a", text: "Legacy" },
      { type: "ModelStreamDelta", run_id: "a", text: "Foreign actor" },
      { type: "ModelStreamDelta", run_id: "a", text: "Empty model" },
    );
    recorded[0].actor = { type: "Agent", agent_id: "agent", run_id: "a", model: "recorded-model" };
    recorded[2].actor = { type: "Agent", agent_id: "agent", run_id: "b", model: "foreign-model" };
    recorded[3].actor = { type: "Agent", agent_id: "agent", run_id: "a", model: "  " };
    const projected = projectSession({ ready: true, cursor: 4, droppedEvents: 0, events: recorded });
    expect(projected.rows.map(row => row.text)).toEqual(["Recorded", "LegacyForeign actorEmpty model"]);
    expect(projected.rows[0].modelId).toBe("recorded-model");
    expect(projected.rows[1]).not.toHaveProperty("modelId");
  });

  it("preserves tool and interleaved agent order when model deltas arrive across updates", () => {
    const initial = projectSession({ ready: true, cursor: 2, droppedEvents: 0, events: events(
      { type: "ModelStreamDelta", run_id: "a", text: "First " },
      { type: "ModelStreamDelta", run_id: "a", text: "answer" },
    ) });
    const next = projectSession({ ready: true, cursor: 6, droppedEvents: 0, events: events(
      { type: "ToolStarted", run_id: "a", tool: "workspace.read_file", args_digest: "digest" },
      { type: "ModelStreamDelta", run_id: "b", text: "Other agent" },
      { type: "ModelStreamDelta", run_id: "a", text: "Then continue" },
      { type: "ModelStreamDelta", run_id: "a", text: "." },
    ).map(event => ({ ...event, sequence: event.sequence + 2 })) }, initial);
    expect(next.rows.map(row => row.text)).toEqual(["First answer", "workspace.read_file", "Other agent", "Then continue."]);
    expect(next.rows.filter(row => row.runId === "a").map(row => row.text)).toEqual(["First answer", "Then continue."]);
  });

  it("keeps failed tools and rollback evidence visible and does not equate run completion with validation", () => {
    const projected = projectSession({ ready: true, cursor: 3, droppedEvents: 0, events: events(
      { type: "ToolCompleted", run_id: "a", tool: "shell.run", outcome: { type: "Failed", message: "Tests failed" } },
      { type: "CheckpointRestored", run_id: "a", checkpoint_id: "checkpoint", restored: false },
      { type: "RunCompleted", run_id: "a", disposition: { type: "Completed" }, chronicle: { id: "chronicle", media_type: "text/plain", byte_length: 0, sha256: "0".repeat(64), sensitivity: { type: "Internal" } } },
    ) });
    expect(projected.rows.map(row => row.text).join("\n")).toContain("Tests failed");
    expect(projected.rows[1].text).toContain("rolled back");
    expect(projected.rows[2].text).toContain("validation not assessed");
  });
});
