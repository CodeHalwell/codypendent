import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { CodeGraphEdgeView, CodeGraphSourceLocation } from "@codypendent/protocol";
import { FunctionConnections } from "../src/components/codeGraph/FunctionConnections.js";
import { functionConnections } from "../src/components/codeGraph/functionRelations.js";
import type { GraphNode } from "../src/components/codeGraph/layout.js";
import { edge, node } from "./code-graph-fixtures.js";

const location = (source_path: string, line: number, column = 1): CodeGraphSourceLocation => ({ source_path, line, column });
const active: GraphNode = { id: "work", name: "app::work", detail: { ...node("work"), location: location("src/work.rs", 12) } };
const caller: GraphNode = { id: "caller", name: "app::caller", detail: { ...node("caller"), location: location("src/caller.rs", 3) } };
const external: GraphNode = { id: "external", name: "dependency::send", detail: { ...node("external"), kind: "external_dependency", source_path: undefined } };
const byId = new Map([[active.id, active], [caller.id, caller], [external.id, external]]);
const calls: CodeGraphEdgeView[] = [
  { ...edge("caller", "work"), call_site: location("src/caller.rs", 20, 4) },
  { ...edge("caller", "work"), revision: "second-copy", call_site: location("src/caller.rs", 20, 4) },
  { ...edge("caller", "work"), revision: "another-recorded-site", call_site: location("src/caller.rs", 35, 2) },
  { ...edge("work", "external"), call_site: location("src/work.rs", 18, 8) },
  edge("work", "not-a-call", "references"),
];

describe("function definition and call presentation", () => {
  it("groups recorded callers once, preserves distinct call locations and excludes other relationships", () => {
    const grouped = functionConnections(active, calls, byId);
    expect(grouped.calledBy).toHaveLength(1);
    expect(grouped.calledBy[0].id).toBe("caller");
    expect(grouped.calledBy[0].locations).toEqual([location("src/caller.rs", 20, 4), location("src/caller.rs", 35, 2)]);
    expect(grouped.calls.map((entry) => entry.id)).toEqual(["external"]);
    expect(grouped.definition).toEqual(location("src/work.rs", 12));
  });

  it("opens exact definition/call positions separately from selecting neighboring functions", async () => {
    const select = vi.fn();
    const open = vi.fn().mockResolvedValue(undefined);
    render(<FunctionConnections active={active} edges={calls} byId={byId} onSelect={select} onOpen={open} />);
    const incoming = screen.getByRole("region", { name: /^Called by/ });
    expect(within(incoming).getAllByRole("button", { name: "caller" })).toHaveLength(1);
    fireEvent.click(within(incoming).getByRole("button", { name: "caller" }));
    expect(select).toHaveBeenCalledExactlyOnceWith("caller");
    fireEvent.click(screen.getByRole("button", { name: "Open definition at src/work.rs:12" }));
    fireEvent.click(screen.getByRole("button", { name: "Open call at src/caller.rs:35:2" }));
    await act(async () => {});
    expect(open.mock.calls.map((args) => args[0])).toEqual([location("src/work.rs", 12), location("src/caller.rs", 35, 2)]);
    expect(screen.getByText("External reference · no local definition recorded")).toBeTruthy();
    expect(screen.queryByText(/not-a-call/)).toBeNull();
  });

  it("keeps loading and partial emptiness distinct from a completed absence of recorded calls", () => {
    const view = render(<FunctionConnections active={active} edges={[]} byId={byId} onSelect={vi.fn()} loading />);
    expect(screen.getByText("Looking for callers…")).toBeTruthy();
    expect(screen.queryByText("No recorded callers.")).toBeNull();
    view.rerender(<FunctionConnections active={active} edges={[]} byId={byId} onSelect={vi.fn()} partial />);
    expect(screen.getByText("No callers in the loaded results.")).toBeTruthy();
    expect(screen.queryByText("No recorded callers.")).toBeNull();
    view.rerender(<FunctionConnections active={active} edges={[]} byId={byId} onSelect={vi.fn()} />);
    expect(screen.getByText("No recorded callers.")).toBeTruthy();
  });

  it("does not invent a definition line or call site and preserves loaded results after an expansion error", () => {
    const unlocated = { ...active, detail: { ...node("work"), location: undefined } };
    render(<FunctionConnections active={unlocated} edges={[edge("caller", "work")]} byId={byId} onSelect={vi.fn()} onOpen={vi.fn()}
      error="Private transport detail" />);
    expect(screen.getByText("src/work.rs")).toBeTruthy();
    expect(screen.getByText(/Line information is not recorded/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "caller" })).toBeTruthy();
    expect(screen.getByText("Call location not recorded.")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /Open definition/ })).toBeNull();
    expect(screen.getByRole("alert").textContent).toContain("Some function connections could not be loaded.");
    expect(screen.getByText("Connection details").closest("details")?.open).toBe(false);
    expect(screen.queryByText("No recorded calls.")).toBeNull();
  });

  it("leaves source-open error presentation to the inspector without an unhandled rejected callback", async () => {
    const open = vi.fn().mockRejectedValue(new Error("Editor unavailable"));
    render(<FunctionConnections active={active} edges={[]} byId={byId} onSelect={vi.fn()} onOpen={open} />);
    fireEvent.click(screen.getByRole("button", { name: "Open definition at src/work.rs:12" }));
    await act(async () => {});
    expect(open).toHaveBeenCalledExactlyOnceWith(location("src/work.rs", 12));
  });
});
