import { fireEvent, render, screen } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import { Transcript } from "../src/components/Transcript.js";
import type { TranscriptItem } from "../src/types.js";

const progress = (text: string): TranscriptItem => ({ id: "progress", type: "thought", timestamp: "", text });

it("prints formatted progress directly and updates it during streaming", () => {
  const view = render(<Transcript items={[progress("**Checking repository contents**")]} connectionStatus="connected" />);
  expect(view.container.querySelector(".transcript-thought strong")).toBeNull();
  expect(screen.getByText("Checking repository contents").closest("details")).toBeNull();
  expect(screen.queryByText("Thought Process")).toBeNull();
  view.rerender(<Transcript items={[progress("**Checking repository contents**\n\nReading `README.md`.")]} connectionStatus="connected" />);
  expect(screen.getByText("README.md").tagName).toBe("CODE");
  expect(view.container.querySelectorAll(".transcript-thought")).toHaveLength(1);
});

it("combines short progress headings into one quiet line", () => {
  const view = render(<Transcript items={[progress("**Checking repository contents**\n\n**Checking repository state**")]} connectionStatus="connected" />);
  expect(view.container.querySelector(".transcript-thought")?.textContent).toBe("Checking repository contents · Checking repository state");
});

it("keeps progress separate from the final answer and omits empty blocks", () => {
  const view = render(<Transcript items={[progress(" \n "), { id: "answer", type: "assistant", text: "Here is the answer.", timestamp: "" }]} connectionStatus="connected" />);
  expect(view.container.querySelectorAll(".transcript-thought")).toHaveLength(0);
  expect(screen.getByText("Here is the answer.").closest(".transcript-assistant")).toBeTruthy();
});

it("removes decorative speaker text and opens citations in the owning historical run", () => {
  const openFile = vi.fn().mockResolvedValue(undefined);
  const view = render(<Transcript items={[{ id: "answer", runId: "historical-run", type: "assistant", timestamp: "", text: "See [agent.py](</repo/agent.py:68>)." },
    { id: "context", type: "system", text: "context carried from the conversation", timestamp: "" }]} connectionStatus="connected" onOpenFile={openFile} />);
  expect(view.container.querySelector(".transcript-speaker")).toBeNull();
  expect(screen.queryByText("context carried from the conversation")).toBeNull();
  fireEvent.click(screen.getByRole("button", { name: "Open agent.py at line 68" }));
  expect(openFile).toHaveBeenCalledWith("/repo/agent.py", 68, "historical-run");
  expect(view.container.textContent).not.toContain("/repo/");
});
