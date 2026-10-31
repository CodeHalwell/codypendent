import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { KnowledgeGraph } from "../src/components/codeGraph/KnowledgeGraph.js";
import { edge, node, page } from "./code-graph-fixtures.js";

const located = (id: string, source_path: string, line: number) => ({ ...node(id), source_path, location: { source_path, line, column: 1 } });
const base = () => page([located("work", "src/work.rs", 12), located("next", "src/next.rs", 4)], [edge("work", "next")]);
const neighbours = () => ({ ...page([located("work", "src/work.rs", 12)], [
  { ...edge("caller", "work"), call_site: { source_path: "src/caller.rs", line: 20, column: 1 } },
  { ...edge("work", "next"), call_site: { source_path: "src/work.rs", line: 15, column: 1 } },
]), total_nodes: 1, related_nodes: [located("caller", "src/caller.rs", 7), located("next", "src/next.rs", 4)] });

describe("code navigation", () => {
  it("loads callers automatically and opens definitions and call sites at their real lines", async () => {
    const read = vi.fn().mockResolvedValue(neighbours()), open = vi.fn().mockResolvedValue(undefined);
    render(<KnowledgeGraph page={base()} defaultMode="calls" readNeighbors={read} onOpenFile={open} />);
    fireEvent.click(screen.getByRole("button", { name: "Inspect app::work" }));
    await screen.findByRole("button", { name: "caller" });
    expect(read).toHaveBeenCalledWith("work");
    // The caller list updates before physics supplies the new node's position.
    await waitFor(() => expect(screen.getAllByTestId("graph-edge")).toHaveLength(2));
    fireEvent.click(screen.getByRole("button", { name: "Open definition at src/work.rs:12" }));
    fireEvent.click(screen.getByRole("button", { name: "Open call at src/caller.rs:20" }));
    await act(async () => {});
    expect(open.mock.calls).toEqual([["src/work.rs", 12], ["src/caller.rs", 20]]);
    const details = screen.getByRole("complementary", { name: "Symbol details" });
    expect(within(details).getByRole("region", { name: /^Called by/ }).textContent).toContain("caller");
    expect(within(details).getByRole("region", { name: /^Calls/ }).textContent).toContain("next");
  });

  it("leaves source-opening failures visible", async () => {
    const read = vi.fn().mockResolvedValue(neighbours());
    render(<KnowledgeGraph page={base()} defaultMode="calls" readNeighbors={read}
      onOpenFile={vi.fn().mockRejectedValue(new Error("Editor is unavailable"))} />);
    fireEvent.click(screen.getByRole("button", { name: "Inspect app::work" }));
    await screen.findByRole("button", { name: "caller" });
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Open definition at src/work.rs:12" })); });
    expect(screen.getByRole("alert").textContent).toContain("Editor is unavailable");
  });

  it("ignores a source-open rejection after selection changes", async () => {
    let reject!: (error: Error) => void;
    const open = vi.fn(() => new Promise<void>((_, fail) => { reject = fail; }));
    render(<KnowledgeGraph page={base()} onOpenFile={open} />);
    fireEvent.click(screen.getByRole("button", { name: "Inspect app::work" }));
    fireEvent.click(screen.getByRole("button", { name: "Open definition at src/work.rs:12" }));
    fireEvent.click(screen.getByRole("button", { name: "Inspect app::next" }));
    await act(async () => { reject(new Error("Old source could not open")); });
    expect(within(screen.getByRole("complementary", { name: "Symbol details" })).getByRole("heading", { name: "next" })).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("ignores an older source-open failure after a newer request succeeds for the same symbol", async () => {
    let reject!: (error: Error) => void;
    const open = vi.fn().mockImplementationOnce(() => new Promise<void>((_, fail) => { reject = fail; }))
      .mockResolvedValueOnce(undefined);
    render(<KnowledgeGraph page={base()} onOpenFile={open} />);
    fireEvent.click(screen.getByRole("button", { name: "Inspect app::work" }));
    const definition = screen.getByRole("button", { name: "Open definition at src/work.rs:12" });
    fireEvent.click(definition);
    await act(async () => { fireEvent.click(definition); });
    await act(async () => { reject(new Error("Obsolete source-open request failed")); });
    expect(open).toHaveBeenCalledTimes(2);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("does not apply a former symbol's late reply to a newer repository query", async () => {
    let finish!: (value: ReturnType<typeof neighbours>) => void;
    const read = vi.fn(() => new Promise<ReturnType<typeof neighbours>>((done) => { finish = done; }));
    const view = render(<KnowledgeGraph page={base()} defaultMode="calls" readNeighbors={read} />);
    fireEvent.click(screen.getByRole("button", { name: "Inspect app::work" }));
    await act(async () => {});
    view.rerender(<KnowledgeGraph page={page([located("fresh", "src/fresh.rs", 2)], [])} defaultMode="calls" />);
    await act(async () => { finish(neighbours()); });
    expect(screen.queryByRole("button", { name: "Inspect app::caller" })).toBeNull();
    expect(screen.getByRole("button", { name: "Inspect app::fresh" })).toBeTruthy();
  });
});
