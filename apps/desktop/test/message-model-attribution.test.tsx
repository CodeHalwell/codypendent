import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { SessionEvent } from "@codypendent/protocol";
import { initialState, reduce, type DaemonState } from "../src/daemonState.js";
import { Transcript } from "../src/components/Transcript.js";
import { Composer } from "../src/components/Composer.js";

function delta(sequence: number, model?: string, runId = "run-a", thought = false): SessionEvent {
  return {
    sequence, occurred_at: "2026-10-26T00:00:00Z",
    actor: model === undefined ? { type: "System" } : { type: "Agent", agent_id: "agent", run_id: runId, model },
    body: { type: "ModelStreamDelta", run_id: runId, text: `${sequence} `, thought },
  };
}

function apply(state: DaemonState, event: SessionEvent): DaemonState {
  return reduce(state, { type: "frame", frame: { kind: "event", session_id: "session", event } });
}

describe("daemon message model attribution", () => {
  it("preserves the exact per-message model through streaming and replay, including switches and sibling runs", () => {
    const events = [delta(1, "acp/codex-acp#gpt-6.1-sol"), delta(2, "acp/codex-acp#gpt-6.1-sol"),
      delta(3, "acp/claude-acp#sonnet"), delta(4, "acp/codex-acp#gpt-6-luna", "run-b"),
      delta(5, "acp/claude-acp#sonnet", "run-a", true), delta(6, "acp/claude-acp#sonnet")];
    const live = events.reduce(apply, initialState);
    const replay = reduce(initialState, { type: "frame", frame: {
      kind: "history", session_id: "session", through: 6, events,
    } });
    expect(replay.transcript).toEqual(live.transcript);
    expect(live.transcript.map(({ type, text, modelId, runId }) => ({ type, text, modelId, runId }))).toEqual([
      { type: "assistant", text: "1 2 ", modelId: "acp/codex-acp#gpt-6.1-sol", runId: "run-a" },
      { type: "assistant", text: "3 ", modelId: "acp/claude-acp#sonnet", runId: "run-a" },
      { type: "assistant", text: "4 ", modelId: "acp/codex-acp#gpt-6-luna", runId: "run-b" },
      { type: "thought", text: "5 ", modelId: "acp/claude-acp#sonnet", runId: "run-a" },
      { type: "assistant", text: "6 ", modelId: "acp/claude-acp#sonnet", runId: "run-a" },
    ]);
  });

  it("never borrows a prior or foreign actor's model when attribution is missing", () => {
    const foreign = delta(3, "wrong-model", "run-b");
    foreign.body = { type: "ModelStreamDelta", run_id: "run-a", text: "3 " };
    const state = [delta(1, "recorded-model"), delta(2), foreign, delta(4, " ")].reduce(apply, initialState);
    expect(state.transcript).toHaveLength(2);
    expect(state.transcript[0].modelId).toBe("recorded-model");
    expect(state.transcript[1]).toMatchObject({ text: "2 3 4 ", modelId: undefined });
  });

  it("labels each response from its daemon record even when the picker has a different model", () => {
    const state = [delta(1, "acp/codex-acp#gpt-6.1-sol"), delta(2, "acp/claude-acp#sonnet")].reduce(apply, initialState);
    const view = render(<><Composer isRunning={false} onSend={() => true} onOpenModels={() => {}} statusLabel="Build mode · acp/codex-acp#gpt-6-luna" />
      <Transcript connectionStatus="connected" items={state.transcript} /></>);
    expect(screen.getByRole("button", { name: "Codex · GPT-6 Luna" })).toBeTruthy();
    expect(screen.getByLabelText("Response model: Codex · GPT-6.1 Sol").getAttribute("title"))
      .toBe("Recorded by daemon: acp/codex-acp#gpt-6.1-sol");
    expect(screen.getByLabelText("Response model: Claude · Sonnet").getAttribute("data-model-id"))
      .toBe("acp/claude-acp#sonnet");
    expect(view.container.querySelectorAll(".transcript-assistant .transcript-model")).toHaveLength(2);
  });

  it("shows unreported identities honestly for old messages and agent defaults", () => {
    const state = [delta(1), delta(2, "acp/codex-acp"), delta(3, "acp/claude-acp#default", "run-b", true)].reduce(apply, initialState);
    render(<Transcript connectionStatus="connected" items={state.transcript} />);
    expect(screen.getByLabelText("Response model: Model not recorded")).toBeTruthy();
    expect(screen.getByLabelText("Response model: Codex · Model not reported")).toBeTruthy();
    expect(screen.getByLabelText("Response model: Claude · Model not reported").closest(".transcript-thought")).toBeTruthy();
  });
});
