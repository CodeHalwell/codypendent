/**
 * Reasoning is not speech.
 *
 * ACP separates `AgentThoughtChunk` from `AgentMessageChunk`; the daemon merged
 * them until v0.12.2, so a model that deliberates out loud printed its whole
 * deliberation as the reply and buried the answer under it. These tests hold
 * the client end: marked chunks land in their own progress entry, unmarked ones
 * behave exactly as they always did.
 */
import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { initialState, reduce, type DaemonState } from "../src/daemonState.js";
import { Transcript } from "../src/components/Transcript.js";

function delta(text: string, sequence: number, thought?: boolean) {
  const body: Record<string, unknown> = { type: "ModelStreamDelta", run_id: "run-1", text };
  if (thought !== undefined) {
    body.thought = thought;
  }
  return {
    type: "frame" as const,
    frame: {
      kind: "event" as const,
      session_id: "session-1",
      event: { sequence, occurred_at: "2026-01-01T00:00:00Z", body: body as never },
    } as never,
  };
}

function attached(): DaemonState {
  return { ...initialState, activeSessionId: "session-1", activeRunId: "run-1" };
}

describe("the reasoning channel", () => {
  it("keeps deliberation out of the reply", () => {
    let state = reduce(attached(), delta("The user said hello. I should ", 1, true));
    state = reduce(state, delta("answer briefly.", 2, true));
    state = reduce(state, delta("Hello! ", 3, false));
    state = reduce(state, delta("What can I help with?", 4, false));

    expect(state.transcript.map((item) => item.type)).toEqual(["thought", "assistant"]);
    expect(state.transcript[0].text).toBe("The user said hello. I should answer briefly.");
    // The reply is clean: none of the deliberation leaked into it.
    expect(state.transcript[1].text).toBe("Hello! What can I help with?");
  });

  it("treats a chunk with no flag as speech, exactly as older daemons sent it", () => {
    const state = reduce(attached(), delta("plain output", 1));
    expect(state.transcript[0].type).toBe("assistant");
  });

  it("does not coalesce reasoning into an adjacent reply", () => {
    let state = reduce(attached(), delta("answering now", 1, false));
    state = reduce(state, delta("second thoughts", 2, true));
    expect(state.transcript.map((item) => item.type)).toEqual(["assistant", "thought"]);
  });

  it("renders reasoning as progress, with the reply as separate page content", () => {
    let state = reduce(attached(), delta("deliberating", 1, true));
    state = reduce(state, delta("the answer", 2, false));
    render(<Transcript items={state.transcript} connectionStatus="connected" />);
    expect(screen.getByText("the answer")).toBeTruthy();
    const progress = screen.getByLabelText("Agent progress");
    expect(within(progress).getByText("deliberating")).toBeTruthy();
    expect(within(progress).queryByText("the answer")).toBeNull();
  });
});
