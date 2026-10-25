import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { SessionLibrary } from "../src/components/SessionLibrary.js";
import type { DesktopTransport, SessionLifecycleOutcome, SessionSearchAnswer } from "../src/transport.js";

afterEach(() => vi.useRealTimers());
const session = { session_id: "session-a", title: "Original title", state: "open", created_at: "2026-09-08", updated_at: "2026-09-08" };
const answer: SessionSearchAnswer = { query: "", cursor: null, page: { items: [{
  session, deep_link: { type: "Session", session_id: session.session_id },
  scope: { type: "Session" }, source: { type: "Title" }, score: 1, stable_identity: "a",
}] } };
function pending() {
  let resolve!: (value: SessionLifecycleOutcome) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<SessionLifecycleOutcome>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
async function open(transport: DesktopTransport) {
  vi.useFakeTimers();
  const view = render(<SessionLibrary transport={transport} />);
  await act(async () => vi.advanceTimersByTime(200));
  return view;
}

describe("session action recovery", () => {
  it("starts delete on Cancel, traps focus, and ignores IME Escape", async () => {
    const mutateSession = vi.fn();
    await open({ searchSessions: async () => answer, mutateSession } as unknown as DesktopTransport);
    const trigger = screen.getByRole("button", { name: "Delete" });
    trigger.focus(); fireEvent.click(trigger);
    const dialog = screen.getByRole("dialog", { name: "Delete this session?" });
    const cancel = within(dialog).getByRole("button", { name: "Cancel" });
    const confirm = within(dialog).getByRole("button", { name: "Delete" });
    expect(document.activeElement).toBe(cancel);
    fireEvent.keyDown(cancel, { key: "Tab", shiftKey: true });
    expect(document.activeElement).toBe(confirm);
    fireEvent.keyDown(confirm, { key: "Tab" });
    expect(document.activeElement).toBe(cancel);
    fireEvent.keyDown(cancel, { key: "Escape", isComposing: true });
    expect(screen.getByRole("dialog")).toBeTruthy();
    fireEvent.keyDown(cancel, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(document.activeElement).toBe(trigger);
    expect(mutateSession).not.toHaveBeenCalled();
  });

  it("retains a refused rename, blocks duplicate sends, and accepts a retry", async () => {
    const first = pending();
    const next = pending();
    const mutateSession = vi.fn().mockReturnValueOnce(first.promise).mockReturnValueOnce(next.promise);
    const renamed = { ...session, title: "Revised title" };
    const searchSessions = vi.fn().mockResolvedValueOnce(answer).mockResolvedValue({ ...answer, page: { items: [{ ...answer.page.items[0], session: renamed }] } });
    await open({ searchSessions, mutateSession } as unknown as DesktopTransport);
    fireEvent.click(screen.getByRole("button", { name: "Rename" }));
    const input = screen.getByRole("textbox", { name: "Rename session" });
    expect(document.activeElement).toBe(input);
    fireEvent.change(input, { target: { value: "Revised title" } });
    fireEvent.keyDown(input, { key: "Enter", isComposing: true });
    expect(mutateSession).not.toHaveBeenCalled();
    act(() => { fireEvent.keyDown(input, { key: "Enter" }); fireEvent.keyDown(input, { key: "Enter" }); });
    expect(mutateSession).toHaveBeenCalledTimes(1);
    expect(document.activeElement).toBe(screen.getByRole("dialog"));
    expect(within(screen.getByRole("dialog")).getByRole("status").textContent).toContain("Waiting");
    await act(async () => first.reject(new Error("Rename refused by daemon")));
    expect(document.activeElement).toBe(input);
    expect((input as HTMLInputElement).value).toBe("Revised title");
    expect(within(screen.getByRole("dialog")).getByRole("alert").textContent).toContain("Rename refused");
    fireEvent.keyDown(input, { key: "Enter" });
    await act(async () => next.resolve({ outcome: "applied", session: renamed }));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.getByText("Revised title")).toBeTruthy();
    expect(searchSessions).toHaveBeenCalledTimes(2);
  });

  it("retires mutation outcomes when the transport is replaced", async () => {
    const old = pending();
    const first = { searchSessions: async () => answer, mutateSession: () => old.promise } as unknown as DesktopTransport;
    const { rerender } = await open(first);
    fireEvent.click(screen.getByRole("button", { name: "Pin" }));
    expect((screen.getByRole("button", { name: "Archive" }) as HTMLButtonElement).disabled).toBe(true);
    rerender(<SessionLibrary transport={{ searchSessions: async () => answer, mutateSession: vi.fn() } as unknown as DesktopTransport} />);
    await act(async () => vi.advanceTimersByTime(200));
    await act(async () => old.resolve({ outcome: "deleted", session_id: session.session_id, tombstoned: false }));
    expect(screen.getByText("Original title")).toBeTruthy();
    expect(screen.queryByText(/deleted/)).toBeNull();
  });

  it("returns focus to search when deletion removes the original trigger", async () => {
    const searchSessions = vi.fn().mockResolvedValueOnce(answer)
      .mockResolvedValue({ query: "", cursor: null, page: { items: [] } });
    const mutateSession = vi.fn().mockResolvedValue({ outcome: "deleted", session_id: session.session_id, tombstoned: true });
    await open({ searchSessions, mutateSession } as unknown as DesktopTransport);
    const trigger = screen.getByRole("button", { name: "Delete" });
    trigger.focus(); fireEvent.click(trigger);
    await act(async () => fireEvent.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Delete" })));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.getByText(/tombstoned by the daemon/)).toBeTruthy();
    expect(document.activeElement).toBe(screen.getByRole("textbox", { name: "Search sessions" }));
  });

  it("offers retry after a failed search and never calls an unavailable transport", async () => {
    const searchSessions = vi.fn().mockRejectedValueOnce(new Error("temporary failure")).mockResolvedValue(answer);
    const transport = { searchSessions } as unknown as DesktopTransport;
    const view = await open(transport);
    expect(screen.getByTestId("library-failed")).toBeTruthy();
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Retry search" })));
    expect(screen.getByText("Original title")).toBeTruthy();
    view.rerender(<SessionLibrary transport={transport} unavailable="Disconnected" />);
    await act(async () => vi.advanceTimersByTime(200));
    expect(searchSessions).toHaveBeenCalledTimes(2);
    expect(screen.getByTestId("library-unavailable").textContent).toContain("Disconnected");
  });
});
