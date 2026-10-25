import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { SessionEvent } from "@codypendent/protocol";
import { initialState, reduce } from "../src/daemonState.js";
import { Transcript, TRANSCRIPT_PAGE_SIZE } from "../src/components/Transcript.js";
import type { TranscriptItem } from "../src/types.js";

const note = (index: number): SessionEvent => ({ sequence: index + 1, occurred_at: "2026-10-04T20:00:00Z",
  body: { type: "NoteAppended", text: `History message ${index + 1}` } } as SessionEvent);
const message = (index: number): TranscriptItem => ({ id: `note-${index + 1}`, type: "system", text: `History message ${index + 1}`, timestamp: "" });
const history = (events: SessionEvent[]) => ({ type: "frame" as const,
  frame: { kind: "history" as const, session_id: "session", through: events.at(-1)?.sequence ?? 0, events } });
const visible = () => document.querySelector("[data-transcript-window]")!;

describe("large history reconstruction", () => {
  it("replays 25,000 notes without mutating an earlier projection or event", () => {
    const events = Array.from({ length: 25_000 }, (_, index) => Object.freeze(note(index)));
    const before = reduce(initialState, history(events.slice(0, 2)));
    Object.freeze(before.transcript);
    for (const item of before.transcript) Object.freeze(item);
    const result = reduce(before, history(events));
    expect(result.transcript).toHaveLength(25_000);
    expect(result.transcript[0].text).toBe("History message 1");
    expect(result.transcript.at(-1)?.text).toBe("History message 25000");
    expect(before.transcript).toHaveLength(2);
    Object.freeze(result.transcript);
    const next = reduce(result, { type: "frame", frame: { kind: "event", session_id: "session", event: note(25_000) } });
    expect(result.transcript).toHaveLength(25_000);
    expect(next.transcript).toHaveLength(25_001);
    expect(next.transcript[0]).toBe(result.transcript[0]);
  });

  it("matches live folding of mixed streaming, tool, backstage and patch events", () => {
    const bodies = [
      { type: "RunStarted", run_id: "run", objective: "Review source" },
      { type: "NoteAppended", run_id: "run", text: "=== CONTEXT\nsource" },
      { type: "ModelStreamDelta", run_id: "run", text: "Check " },
      { type: "ModelStreamDelta", run_id: "run", text: "this" },
      { type: "ToolStarted", run_id: "run", tool: "shell.run", label: "Run tests" },
      { type: "ToolCompleted", run_id: "run", tool: "shell.run", outcome: { type: "Succeeded" } },
      { type: "NoteAppended", run_id: "run", text: "remembered: use Python" },
      { type: "PatchProposed", run_id: "run", files: ["a.py"], additions: 1, deletions: 0,
        artifact: { id: "patch" }, preview: "--- /dev/null\n+++ b/a.py\n@@ -0,0 +1 @@\n+ok\n" },
      { type: "ApprovalRequested", approval_id: "approval", action: { type: "ExecuteCommand", program: "python3", args: [] } },
      { type: "ApprovalResolved", approval_id: "approval" },
      { type: "RunCompleted", run_id: "run", disposition: { type: "Completed" } },
    ];
    const events = bodies.map((body, index) => ({ ...note(index), body } as SessionEvent));
    const live = events.reduce((state, event) => reduce(state, { type: "frame", frame: { kind: "event", session_id: "session", event } }), initialState);
    const replay = reduce(initialState, history(events));
    expect(replay.transcript).toEqual(live.transcript);
    expect(replay.activity).toEqual(live.activity);
    expect(replay.runState).toEqual(live.runState);
    expect(replay.transcript.find((item) => item.patchFiles)?.patchRunId).toBe("run");
  });
});

describe("bounded transcript history", () => {
  it.each([0, 2])("positions initial bulk history at the bottom before animation frames (previous rows: %s)", (count) => {
    const animation = vi.spyOn(window, "requestAnimationFrame").mockReturnValue(1);
    try {
      const view = render(<Transcript items={Array.from({ length: count }, (_, index) => message(index))} connectionStatus="connected" />);
      Object.defineProperty(visible(), "scrollHeight", { value: 4_000 });
      view.rerender(<Transcript items={Array.from({ length: 25_000 }, (_, index) => message(index))} connectionStatus="connected" />);
      expect(visible().scrollTop).toBe(4_000);
      expect((screen.getByRole("button", { name: "Latest" }) as HTMLButtonElement).disabled).toBe(true);
    } finally {
      animation.mockRestore();
    }
  });

  it("mounts only 200 of 25,000 rows and can reach both ends and intermediate pages", () => {
    render(<Transcript items={Array.from({ length: 25_000 }, (_, index) => message(index))} connectionStatus="connected" />);
    expect(visible().children).toHaveLength(TRANSCRIPT_PAGE_SIZE);
    expect(screen.queryByText("History message 1")).toBeNull();
    expect(screen.getByText("History message 25000")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Older" }));
    expect(visible().getAttribute("data-transcript-window")).toBe("24600:24800");
    fireEvent.click(screen.getByRole("button", { name: "First" }));
    expect(screen.getByText("History message 1")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Newer" }));
    expect(visible().getAttribute("data-transcript-window")).toBe("200:400");
    fireEvent.click(screen.getByRole("button", { name: "Latest" }));
    expect(visible().getAttribute("data-transcript-window")).toBe("24800:25000");
    expect(visible().children).toHaveLength(TRANSCRIPT_PAGE_SIZE);
  });

  it("positions new accepted-turn rows before deferred frames and preserves a reader during background output", () => {
    const animation = vi.spyOn(window, "requestAnimationFrame").mockReturnValue(1);
    try {
      const items = Array.from({ length: 20 }, (_, index) => message(index));
      const view = render(<Transcript items={items} connectionStatus="connected" followLatestRequest={0} />);
      const list = visible();
      Object.defineProperties(list, { scrollHeight: { value: 4_000 }, clientHeight: { value: 500 } });
      fireEvent.scroll(list, { target: { scrollTop: 100 } });
      const incoming = [...items, message(20)];
      view.rerender(<Transcript items={incoming} connectionStatus="connected" followLatestRequest={0} />);
      expect(list.scrollTop).toBe(100);
      view.rerender(<Transcript items={incoming} connectionStatus="connected" followLatestRequest={1} />);
      expect(list.scrollTop).toBe(4_000);
      // The accepted command can reply before the daemon's new rows arrive.
      list.scrollTop = 3_500;
      view.rerender(<Transcript items={[...incoming, message(21)]} connectionStatus="connected" followLatestRequest={1} />);
      expect(list.scrollTop).toBe(4_000);
      fireEvent.scroll(list, { target: { scrollTop: 100 } });
      view.rerender(<Transcript items={[...incoming, message(21), message(22)]} connectionStatus="connected" followLatestRequest={1} />);
      expect(list.scrollTop).toBe(100);
    } finally { animation.mockRestore(); }
  });

  it("anchors older history during live appends and removals before the page", () => {
    const items = Array.from({ length: 650 }, (_, index) => message(index));
    const view = render(<Transcript items={items} connectionStatus="connected" />);
    fireEvent.click(screen.getByRole("button", { name: "Older" }));
    expect(visible().firstElementChild?.textContent).toBe("History message 251");
    view.rerender(<Transcript items={[...items.slice(1), message(650)]} connectionStatus="connected" />);
    expect(visible().firstElementChild?.textContent).toBe("History message 251");
    expect(visible().lastElementChild?.textContent).toBe("History message 450");
    fireEvent.click(screen.getByRole("button", { name: "Latest" }));
    expect(screen.getByText("History message 651")).toBeTruthy();
  });

  it("reveals and focuses an off-page request without stealing focus on live appends", () => {
    const items = Array.from({ length: 650 }, (_, index) => message(index));
    items[20] = { id: "approval", type: "approval", text: "Review this command", approvalId: "a", timestamp: "" };
    const view = render(<Transcript items={items} connectionStatus="connected" focusDecision="Approval:a" onApprove={vi.fn()} />);
    expect(visible().getAttribute("data-transcript-window")).toBe("0:200");
    expect(document.activeElement?.getAttribute("data-decision-id")).toBe("Approval:a");
    screen.getByRole("button", { name: "Newer" }).focus();
    view.rerender(<Transcript items={[...items, message(650)]} connectionStatus="connected" focusDecision="Approval:a" onApprove={vi.fn()} />);
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Newer" }));
    expect(visible().getAttribute("data-transcript-window")).toBe("0:200");
  });

  it("keeps the older page when its anchor approval resolves, then anchors its successor", () => {
    const items = Array.from({ length: 650 }, (_, index) => message(index));
    items[250] = { id: "approval", type: "approval", text: "Review this command", approvalId: "a", timestamp: "" };
    const view = render(<Transcript items={items} connectionStatus="connected" />);
    fireEvent.click(screen.getByRole("button", { name: "Older" }));
    expect(visible().getAttribute("data-transcript-window")).toBe("250:450");
    const resolved = items.filter((item) => item.id !== "approval");
    view.rerender(<Transcript items={resolved} connectionStatus="connected" />);
    expect(visible().getAttribute("data-transcript-window")).toBe("250:450");
    expect(visible().firstElementChild?.textContent).toBe("History message 252");
    view.rerender(<Transcript items={[...resolved.slice(1), message(650)]} connectionStatus="connected" />);
    expect(visible().getAttribute("data-transcript-window")).toBe("249:449");
    expect(visible().firstElementChild?.textContent).toBe("History message 252");
  });

  it("clamps a removed latest-page anchor to shorter history and preserves explicit follow", () => {
    const items = Array.from({ length: 650 }, (_, index) => message(index));
    const view = render(<Transcript items={items} connectionStatus="connected" />);
    Object.defineProperties(visible(), { scrollHeight: { value: 4_000 }, clientHeight: { value: 500 } });
    fireEvent.scroll(visible(), { target: { scrollTop: 100 } });
    const shorter = items.slice(0, 350);
    view.rerender(<Transcript items={shorter} connectionStatus="connected" />);
    expect(visible().getAttribute("data-transcript-window")).toBe("150:350");
    view.rerender(<Transcript items={[...shorter, message(350)]} connectionStatus="connected" />);
    expect(visible().getAttribute("data-transcript-window")).toBe("150:350");
    fireEvent.click(screen.getByRole("button", { name: "Latest" }));
    expect(visible().getAttribute("data-transcript-window")).toBe("151:351");
    expect(visible().children).toHaveLength(TRANSCRIPT_PAGE_SIZE);
  });

  it("resumes following live messages after scrolling back to the bottom", () => {
    const items = Array.from({ length: 650 }, (_, index) => message(index));
    const view = render(<Transcript items={items} connectionStatus="connected" />);
    const list = visible();
    Object.defineProperties(list, { scrollHeight: { value: 4_000 }, clientHeight: { value: 500 } });
    fireEvent.scroll(list, { target: { scrollTop: 100 } });
    fireEvent.scroll(list, { target: { scrollTop: 3_500 } });
    view.rerender(<Transcript items={[...items, message(650)]} connectionStatus="connected" />);
    expect(visible().getAttribute("data-transcript-window")).toBe("451:651");
    expect(screen.getByText("History message 651")).toBeTruthy();
  });

  it("keeps a focused latest-page decision mounted when the stream grows", () => {
    const items = Array.from({ length: 650 }, (_, index) => message(index));
    items[450] = { id: "approval", type: "approval", text: "Review this command", approvalId: "a", timestamp: "" };
    const view = render(<Transcript items={items} connectionStatus="connected" focusDecision="Approval:a" />);
    const card = document.activeElement;
    view.rerender(<Transcript items={[...items, message(650)]} connectionStatus="connected" focusDecision="Approval:a" />);
    expect(document.activeElement).toBe(card);
    expect(visible().getAttribute("data-transcript-window")).toBe("450:650");
  });

  it("keeps an open patch on an older page mounted while new messages arrive", () => {
    const items = Array.from({ length: 650 }, (_, index) => message(index));
    items[260] = { id: "patch", type: "system", text: "Review patch", timestamp: "", patchFiles: ["a.py"], patchRunId: "original",
      diffPreview: "--- /dev/null\n+++ b/a.py\n@@ -0,0 +1 @@\n+ok\n" };
    const open = vi.fn().mockResolvedValue(undefined);
    const view = render(<Transcript items={items} connectionStatus="connected" onOpenFile={open} />);
    fireEvent.click(screen.getByRole("button", { name: "Older" }));
    const fold = screen.getByText("Review patch").closest("details")!;
    fireEvent.click(fold.querySelector("summary")!);
    fold.open = true;
    fireEvent(fold, new Event("toggle"));
    view.rerender(<Transcript items={[...items, message(650)]} connectionStatus="connected" onOpenFile={open} />);
    expect(screen.getByText("Review patch").closest("details")).toBe(fold);
    expect(fold.open).toBe(true);
  });
});
