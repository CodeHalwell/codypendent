import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { AnalyticsExportResult, AnalyticsPage, AnalyticsQuery } from "@codypendent/protocol";
import { AnalyticsDashboard } from "../src/components/AnalyticsDashboard.js";

function pending<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const page = (name: string, next_cursor?: string): AnalyticsPage => ({
  items: [{ dimensions: [name], metrics: { input_tokens: 12, cost_micros: 11 } }], next_cursor,
});
const change = async (value: string) => act(async () => {
  fireEvent.change(screen.getByLabelText("Group By:"), { target: { value } });
});

describe("analytics request recovery", () => {
  it("rejects old results when grouping returns to the same value", async () => {
    const query = vi.fn().mockResolvedValue(page("initial"));
    render(<AnalyticsDashboard onQueryAnalytics={query} />);
    await act(async () => undefined);
    // All Time makes the first/third serialized query exactly equal.
    await act(async () => fireEvent.change(screen.getByLabelText("Window:"), { target: { value: "all" } }));
    const oldA = pending<AnalyticsPage>();
    const newA = pending<AnalyticsPage>();
    query.mockReset().mockReturnValueOnce(oldA.promise).mockResolvedValueOnce(page("B"))
      .mockReturnValueOnce(newA.promise);
    await change("provider");
    await change("model");
    await change("provider");
    await act(async () => newA.resolve(page("new A")));
    await act(async () => oldA.resolve(page("stale A")));
    expect(screen.getByText("new A")).toBeTruthy();
    expect(screen.queryByText("stale A")).toBeNull();
  });

  it("requeries a replaced connection and ignores its late error", async () => {
    const old = pending<AnalyticsPage>();
    const query = vi.fn().mockReturnValueOnce(old.promise).mockResolvedValueOnce(page("new daemon"));
    const { rerender } = render(<AnalyticsDashboard onQueryAnalytics={query} connectionEpoch={1} />);
    rerender(<AnalyticsDashboard onQueryAnalytics={query} connectionEpoch={2} />);
    await act(async () => undefined);
    await act(async () => old.reject(new Error("old daemon failed")));
    expect(screen.getByText("new daemon")).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("keeps the exact time window across pages and retries failed continuations", async () => {
    const more = pending<AnalyticsPage>();
    const query = vi.fn<(query?: AnalyticsQuery) => Promise<AnalyticsPage>>()
      .mockResolvedValueOnce(page("first", "cursor-1"))
      .mockReturnValueOnce(more.promise)
      .mockResolvedValueOnce(page("second"));
    render(<AnalyticsDashboard onQueryAnalytics={query} />);
    await act(async () => undefined);
    const first = query.mock.calls[0][0];
    expect(screen.getByRole("cell", { name: "First" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Load more measurements" }));
    expect(query.mock.calls[1][0]).toEqual({ ...first, cursor: "cursor-1" });
    await act(async () => more.reject(new Error("temporary socket failure")));
    expect(screen.getByRole("cell", { name: "First" })).toBeTruthy();
    expect(screen.getByRole("alert").textContent).toContain("retry below");
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Load more measurements" })));
    expect(query.mock.calls[2][0]).toEqual({ ...first, cursor: "cursor-1" });
    expect(screen.getByRole("cell", { name: "First" })).toBeTruthy();
    expect(screen.getByRole("cell", { name: "Second" })).toBeTruthy();
    expect(screen.getByTestId("metric-input-tokens").textContent).toContain("24");
    expect(screen.queryByRole("button", { name: "Load more measurements" })).toBeNull();
  });

  it("drops an old continuation and clears old rows while new filters load", async () => {
    const more = pending<AnalyticsPage>();
    const fresh = pending<AnalyticsPage>();
    const query = vi.fn().mockResolvedValueOnce(page("first", "next"))
      .mockReturnValueOnce(more.promise).mockReturnValueOnce(fresh.promise);
    render(<AnalyticsDashboard onQueryAnalytics={query} />);
    await act(async () => undefined);
    fireEvent.click(screen.getByRole("button", { name: "Load more measurements" }));
    await change("provider");
    expect(screen.queryByRole("cell", { name: "First" })).toBeNull();
    expect(screen.queryByTestId("analytics-empty")).toBeNull();
    expect(screen.getByTestId("analytics-loading")).toBeTruthy();
    await act(async () => fresh.resolve(page("fresh")));
    await act(async () => more.resolve(page("old continuation")));
    expect(screen.queryByText("old continuation")).toBeNull();
    expect(screen.getByText("fresh")).toBeTruthy();
  });

  it("does not present an old export as belonging to new filters", async () => {
    const exportRequest = pending<AnalyticsExportResult>();
    const exportAnalytics = vi.fn().mockReturnValue(exportRequest.promise);
    render(<AnalyticsDashboard onQueryAnalytics={async () => page("current")} onExportAnalytics={exportAnalytics} />);
    await act(async () => undefined);
    const button = screen.getByRole("button", { name: "Export JSON" });
    act(() => { fireEvent.click(button); fireEvent.click(button); });
    expect(exportAnalytics).toHaveBeenCalledTimes(1);
    await change("provider");
    await act(async () => exportRequest.reject(new Error("obsolete export failure")));
    expect(screen.queryByRole("alert")).toBeNull();
    expect((screen.getByRole("button", { name: "Export JSON" }) as HTMLButtonElement).disabled).toBe(false);
  });

  it("labels partial bucket measurements and preserves tiny costs", () => {
    render(<AnalyticsDashboard initialPage={{ items: [...page("measured").items, { dimensions: ["unknown"], metrics: {} }] }} />);
    expect(screen.getByTestId("metric-cost").textContent).toContain("$0.000011");
    expect(screen.getByTestId("metric-cost").textContent).toContain("Partial: 1 of 2");
    expect(screen.getByTestId("metric-output-tokens").textContent).toContain("unavailable");
    expect((screen.getByRole("button", { name: "Refresh" }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole("button", { name: "Export CSV" }) as HTMLButtonElement).disabled).toBe(true);
  });
});
