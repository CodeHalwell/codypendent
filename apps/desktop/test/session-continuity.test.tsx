import { act, fireEvent, render, renderHook, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { App } from "../src/App.js";
import { useDaemon } from "../src/useDaemon.js";
import type { DesktopTransport } from "../src/transport.js";
import { ContinuityTransport } from "./fixtures/ContinuityTransport.js";

const native = (fixture: ContinuityTransport) => fixture as unknown as DesktopTransport;

describe("desktop conversation continuity", () => {
  it("keeps the first message and reply when the next objective uses the confirmed session", async () => {
    const transport = new ContinuityTransport();
    const daemon = renderHook(() => useDaemon(() => native(transport)));
    await waitFor(() => expect(daemon.result.current.state.status).toBe("connected"));
    await act(async () => { await daemon.result.current.submit("Hello"); });
    await act(async () => { await daemon.result.current.submit("Review this project"); });
    expect(transport.starts).toEqual([{ objective: "Hello", sessionId: null }, { objective: "Review this project", sessionId: "session-1" }]);
    expect(daemon.result.current.state.activeSessionId).toBe("session-1");
    expect(daemon.result.current.state.transcript.filter((item) => item.type === "user").map((item) => item.text)).toEqual(["Hello", "Review this project"]);
    expect(daemon.result.current.state.transcript.filter((item) => item.type === "assistant").map((item) => item.text)).toEqual(["Reply to: Hello", "Reply to: Review this project"]);
    expect(transport.sessions).toHaveLength(1);
  });

  it("creates a separate session only after New chat, and ignores delayed old-session frames", async () => {
    const transport = new ContinuityTransport();
    const daemon = renderHook(() => useDaemon(() => native(transport)));
    await waitFor(() => expect(daemon.result.current.state.status).toBe("connected"));
    await act(async () => { await daemon.result.current.submit("Hello"); });
    act(() => { expect(daemon.result.current.newSession()).toBe(true); });
    act(() => { transport.emit("session-1", { type: "NoteAppended", text: "Delayed old note" }); });
    expect(daemon.result.current.state.activeSessionId).toBeNull();
    expect(daemon.result.current.state.transcript).toHaveLength(0);
    await act(async () => { await daemon.result.current.submit("Separate task"); });
    expect(transport.starts.at(-1)?.sessionId).toBeNull();
    expect(daemon.result.current.state.activeSessionId).toBe("session-2");
    expect(transport.sessions).toHaveLength(2);
    await act(async () => { await daemon.result.current.selectSession("session-1"); });
    expect(daemon.result.current.state.transcript.some((item) => item.text === "Reply to: Hello")).toBe(true);
  });

  it("blocks New chat during an outstanding launch without clearing its run", async () => {
    const transport = new ContinuityTransport();
    transport.holdNext = true;
    const daemon = renderHook(() => useDaemon(() => native(transport)));
    await waitFor(() => expect(daemon.result.current.state.status).toBe("connected"));
    let launched!: Promise<boolean>;
    await act(async () => { launched = daemon.result.current.submit("Held task"); });
    act(() => { expect(daemon.result.current.newSession()).toBe(false); });
    await act(async () => { transport.release(); await launched; });
    expect(daemon.result.current.state.activeSessionId).toBe("session-1");
    expect(daemon.result.current.state.transcript.some((item) => item.text === "Held task")).toBe(true);
  });

  it("offers New chat and the keyboard shortcut while preserving the unsent draft", async () => {
    const transport = new ContinuityTransport();
    render(<App makeTransport={() => native(transport)} />);
    await waitFor(() => expect(screen.getByRole("button", { name: "Send" })).toBeTruthy());
    const draft = screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.change(draft, { target: { value: "Hello" } });
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Send" })); });
    fireEvent.change(draft, { target: { value: "Keep my draft" } });
    fireEvent.click(screen.getByRole("button", { name: "New chat" }));
    expect(draft.value).toBe("Keep my draft");
    expect(screen.queryByText("Reply to: Hello")).toBeNull();
    expect(document.activeElement).toBe(draft);
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Send" })); });
    fireEvent.change(draft, { target: { value: "Keyboard draft" } });
    fireEvent.keyDown(window, { key: "n", metaKey: true });
    expect(draft.value).toBe("Keyboard draft");
    expect(screen.queryByText("Reply to: Keep my draft")).toBeNull();
    expect(transport.starts.at(-1)?.sessionId).toBeNull();
  });

  it.each([false, true])("returns to the latest accepted turn with deferred animation frames (refused: %s)", async (refused) => {
    const animation = vi.spyOn(window, "requestAnimationFrame").mockReturnValue(1);
    try {
      const transport = new ContinuityTransport();
      render(<App makeTransport={() => native(transport)} />);
      await waitFor(() => expect(screen.getByRole("button", { name: "Send" })).toBeTruthy());
      const draft = screen.getByRole("textbox") as HTMLTextAreaElement;
      fireEvent.change(draft, { target: { value: "Hello" } });
      await act(async () => fireEvent.click(screen.getByRole("button", { name: "Send" })));
      act(() => {
        for (let index = 0; index < 650; index++) {
          transport.emit("session-1", { type: "NoteAppended", text: `Older message ${index}` });
        }
      });
      fireEvent.click(screen.getByRole("button", { name: "Older" }));
      const list = document.querySelector<HTMLDivElement>("[data-transcript-window]")!;
      Object.defineProperties(list, { scrollHeight: { value: 4_000 }, clientHeight: { value: 500 } });
      list.scrollTop = 100;
      const olderWindow = list.dataset.transcriptWindow;
      transport.failNext = refused;
      fireEvent.change(draft, { target: { value: "My next turn" } });
      await act(async () => fireEvent.click(screen.getByRole("button", { name: "Send" })));
      if (refused) {
        expect(list.dataset.transcriptWindow).toBe(olderWindow);
        expect(list.scrollTop).toBe(100);
        expect(draft.value).toBe("My next turn");
        expect(screen.queryByText("Reply to: My next turn")).toBeNull();
      } else {
        expect(screen.getByText("My next turn")).toBeTruthy();
        expect(screen.getByText("Reply to: My next turn")).toBeTruthy();
        expect((screen.getByRole("button", { name: "Latest" }) as HTMLButtonElement).disabled).toBe(true);
        expect(list.scrollTop).toBe(4_000);
        expect(draft.value).toBe("");
      }
    } finally { animation.mockRestore(); }
  });
});
