import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { SessionLibrary } from "../src/components/SessionLibrary.js";
import type { DesktopTransport, SessionSearchAnswer } from "../src/transport.js";

afterEach(() => vi.useRealTimers());
function pending() {
  let resolve!: (answer: SessionSearchAnswer) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<SessionSearchAnswer>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const empty = (query: string): SessionSearchAnswer => ({ query, cursor: null, page: { items: [] } });
async function type(query: string) {
  fireEvent.change(screen.getByLabelText("Search sessions"), { target: { value: query } });
  await act(async () => vi.advanceTimersByTime(200));
}

describe("session search generations", () => {
  it("ignores an old A response after searching A, B, then A again", async () => {
    vi.useFakeTimers();
    const old = pending();
    const fresh = pending();
    const searchSessions = vi.fn().mockReturnValueOnce(old.promise).mockResolvedValueOnce(empty("B")).mockReturnValueOnce(fresh.promise);
    render(<SessionLibrary transport={{ searchSessions } as unknown as DesktopTransport} />);
    await type("A");
    await type("B");
    await type("A");
    await act(async () => fresh.resolve(empty("A")));
    expect(screen.getByTestId("library-empty").textContent).toContain("A");
    await act(async () => old.reject(new Error("stale A failed")));
    expect(screen.queryByText(/stale A failed/)).toBeNull();
    expect(screen.getByTestId("library-empty")).toBeTruthy();
  });

  it("discards both results and errors from a replaced transport", async () => {
    vi.useFakeTimers();
    const old = pending();
    const first = { searchSessions: () => old.promise } as unknown as DesktopTransport;
    const second = { searchSessions: vi.fn().mockResolvedValue(empty("")) } as unknown as DesktopTransport;
    const { rerender } = render(<SessionLibrary transport={first} />);
    await act(async () => vi.advanceTimersByTime(200));
    rerender(<SessionLibrary transport={second} />);
    await act(async () => vi.advanceTimersByTime(200));
    await act(async () => old.reject(new Error("old connection failed")));
    expect(screen.queryByText(/old connection failed/)).toBeNull();
    expect(screen.getByTestId("library-empty")).toBeTruthy();
  });
});

it("never appends a continuation from an earlier visit to the same query", async () => {
  vi.useFakeTimers();
  const stale = pending();
  const hit = (title: string): SessionSearchAnswer => ({ query: "A", cursor: null, page: { next_cursor: "next", items: [{
    deep_link: { type: "Session", session_id: title }, scope: { type: "Session" }, score: 1,
    source: { type: "Title" }, stable_identity: title,
    session: { session_id: title, title, state: "open", created_at: "2026-09-08", updated_at: "2026-09-08" },
  }] } });
  const searchSessions = vi.fn().mockResolvedValueOnce(hit("first result")).mockReturnValueOnce(stale.promise)
    .mockResolvedValueOnce(empty("B")).mockResolvedValueOnce(hit("fresh result"));
  render(<SessionLibrary transport={{ searchSessions } as unknown as DesktopTransport} />);
  await type("A");
  fireEvent.click(screen.getByRole("button", { name: "Load the next page" }));
  await type("B");
  await type("A");
  await act(async () => stale.resolve({ ...hit("stale continuation"), cursor: "next" }));
  expect(screen.queryByText("stale continuation")).toBeNull();
  expect(screen.getByText("fresh result")).toBeTruthy();
});
