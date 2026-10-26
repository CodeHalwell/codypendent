import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { KnowledgeGraph } from "../src/components/codeGraph/KnowledgeGraph.js";
import { MAP_NODE_LIMIT } from "../src/components/codeGraph/layout.js";
import { edge, node, page } from "./code-graph-fixtures.js";

describe("interactive code knowledge graph", () => {
  it("draws only recorded directed edges and exposes real symbol relationships", () => {
    render(<KnowledgeGraph page={page()} />);
    expect(screen.getAllByTestId("graph-edge")).toHaveLength(2);
    expect(screen.getAllByTestId("graph-edge")[0].getAttribute("marker-end")).toMatch(/^url\(#graph-arrow-/);
    fireEvent.click(screen.getByRole("button", { name: "Inspect app::work" }));
    const details = screen.getByRole("complementary", { name: "Symbol details" });
    expect(within(details).getByText("src/work.rs")).toBeTruthy();
    expect(within(details).getByText("← calls")).toBeTruthy();
    expect(within(details).getByText("→ references")).toBeTruthy();
    fireEvent.change(screen.getByLabelText("Filter connections"), { target: { value: "references" } });
    expect(screen.getAllByTestId("graph-edge")).toHaveLength(1);
    expect(screen.queryByRole("button", { name: "Inspect app::start" })).toBeNull();
  });
  it("supports keyboard pan, zoom, fit and an accessible searchable list", () => {
    render(<KnowledgeGraph page={page()} />);
    const initial = screen.getByTestId("graph-camera").getAttribute("transform");
    fireEvent.click(screen.getByRole("button", { name: "Zoom in" }));
    expect(screen.getByText("125%")).toBeTruthy();
    fireEvent.keyDown(screen.getByRole("group", { name: "Interactive code relationship map" }), { key: "ArrowLeft" });
    expect(screen.getByTestId("graph-camera").getAttribute("transform")).not.toBe(initial);
    fireEvent.click(screen.getByRole("button", { name: "Fit" }));
    expect(screen.getByTestId("graph-camera").getAttribute("transform")).toBe(initial);
    fireEvent.click(screen.getByRole("button", { name: "List" }));
    fireEvent.change(screen.getByLabelText("Find loaded symbols"), { target: { value: "finish" } });
    const list = screen.getByRole("list", { name: "Loaded code symbols" });
    expect(within(list).getAllByRole("button")).toHaveLength(1);
    fireEvent.click(within(list).getByRole("button"));
    expect(screen.getByText("Load its connections to see source details.")).toBeTruthy();
  });
  it("loads real neighbor metadata and makes failures visible without losing the map", async () => {
    const read = vi.fn().mockRejectedValueOnce(new Error("Workspace connection closed")).mockResolvedValueOnce(page([node("finish"), node("next")], [edge("finish", "next")]));
    render(<KnowledgeGraph page={page()} readNeighbors={read} />);
    fireEvent.click(screen.getByRole("button", { name: "Inspect app::finish" }));
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Explore connections" })); });
    expect(screen.getByText("Workspace connection closed")).toBeTruthy();
    expect(screen.getAllByTestId("graph-edge")).toHaveLength(2);
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Explore connections" })); });
    expect(read).toHaveBeenLastCalledWith("finish");
    expect(screen.getByText("src/finish.rs")).toBeTruthy();
    expect(screen.getAllByTestId("graph-edge")).toHaveLength(3);
  });
  it("ignores exploration replies from a previous repository search", async () => {
    let resolve!: (value: ReturnType<typeof page>) => void;
    const read = vi.fn(() => new Promise<ReturnType<typeof page>>((done) => { resolve = done; }));
    const view = render(<KnowledgeGraph page={page()} readNeighbors={read} />);
    fireEvent.click(screen.getByRole("button", { name: "Inspect app::finish" }));
    fireEvent.click(screen.getByRole("button", { name: "Explore connections" }));
    view.rerender(<KnowledgeGraph page={page([node("different")], [])} readNeighbors={read} />);
    await act(async () => { resolve(page([node("old")], [])); });
    expect(screen.queryByRole("button", { name: "Inspect app::old" })).toBeNull();
    expect(screen.getByRole("button", { name: "Inspect app::different" })).toBeTruthy();
  });

  it("keeps a fetched neighbor selected and retryable while its own connections are delayed or fail", async () => {
    let reject!: (error: Error) => void;
    const read = vi.fn().mockResolvedValueOnce(page([node("start"), node("neighbor")], [edge("start", "neighbor")]))
      .mockImplementationOnce(() => new Promise((_, fail) => { reject = fail; }))
      .mockResolvedValueOnce(page([node("neighbor"), node("next")], [edge("neighbor", "next")]));
    render(<KnowledgeGraph page={page([node("start")], [])} readNeighbors={read} defaultMode="calls" />);
    await waitFor(() => expect(screen.getByRole("button", { name: "Inspect app::neighbor" })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Inspect app::neighbor" }));
    await waitFor(() => expect(read).toHaveBeenLastCalledWith("neighbor"));
    const inspector = screen.getByRole("complementary", { name: "Symbol details" });
    expect(within(inspector).getByRole("heading", { name: "neighbor" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Inspect app::neighbor" }).getAttribute("aria-pressed")).toBe("true");
    const camera = screen.getByTestId("graph-camera").getAttribute("transform");
    await act(async () => { reject(new Error("Neighbor connection unavailable")); });
    expect(screen.getByRole("button", { name: "Retry connections" })).toBeTruthy();
    expect(within(inspector).getByText("Neighbor connection unavailable")).toBeTruthy();
    expect(screen.getByTestId("graph-camera").getAttribute("transform")).toBe(camera);
    fireEvent.click(screen.getByRole("button", { name: "Retry connections" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Inspect app::next" })).toBeTruthy());
    expect(screen.getByRole("button", { name: "Inspect app::neighbor" }).getAttribute("aria-pressed")).toBe("true");
  });

  it("ignores late automatic connection replies after another selection and a repository change", async () => {
    let resolveOld!: (value: ReturnType<typeof page>) => void;
    let resolveCurrent!: (value: ReturnType<typeof page>) => void;
    const read = vi.fn().mockResolvedValueOnce(page([node("start"), node("left"), node("right")], [edge("start", "left"), edge("start", "right")]))
      .mockImplementationOnce(() => new Promise((done) => { resolveOld = done; }))
      .mockImplementationOnce(() => new Promise((done) => { resolveCurrent = done; }))
      .mockResolvedValueOnce(page([node("different")], []));
    const view = render(<KnowledgeGraph page={page([node("start")], [])} readNeighbors={read} defaultMode="calls" />);
    await waitFor(() => expect(screen.getByRole("button", { name: "Inspect app::left" })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Inspect app::left" }));
    await waitFor(() => expect(read).toHaveBeenLastCalledWith("left"));
    fireEvent.click(screen.getByRole("button", { name: "List" }));
    fireEvent.click(within(screen.getByRole("list", { name: "Loaded code symbols" })).getByRole("button", { name: /app::right/ }));
    await waitFor(() => expect(read).toHaveBeenLastCalledWith("right"));
    await act(async () => { resolveOld(page([node("left"), node("stale")], [edge("left", "stale")])); });
    expect(within(screen.getByRole("complementary", { name: "Symbol details" })).getByRole("heading", { name: "right" })).toBeTruthy();
    expect(screen.queryByText("app::stale")).toBeNull();
    view.rerender(<KnowledgeGraph page={page([node("different")], [])} readNeighbors={read} defaultMode="calls" />);
    await act(async () => { resolveCurrent(page([node("right"), node("wrong-repository")], [edge("right", "wrong-repository")])); });
    expect(screen.queryByText("app::wrong-repository")).toBeNull();
    expect(screen.queryByText("app::right")).toBeNull();
    expect(screen.queryByText("app::left")).toBeNull();
  });

  it("promotes a List selection beyond the map cap while preserving the camera and node limit", () => {
    const nodes = Array.from({ length: MAP_NODE_LIMIT + 8 }, (_, i) => node(`symbol-${String(i).padStart(3, "0")}`));
    const edges = nodes.slice(1).map((entry) => edge(nodes[0].id, entry.id));
    render(<KnowledgeGraph page={page(nodes, edges)} />);
    fireEvent.click(screen.getByRole("button", { name: "Pause physics" }));
    const target = nodes[nodes.length - 1];
    expect(screen.queryByRole("button", { name: `Inspect ${target.qualified_name}` })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Zoom in" }));
    const camera = screen.getByTestId("graph-camera").getAttribute("transform");
    fireEvent.click(screen.getByRole("button", { name: "List" }));
    const list = screen.getByRole("list", { name: "Loaded code symbols" });
    fireEvent.click(within(list).getByRole("button", { name: new RegExp(target.qualified_name) }));
    fireEvent.click(screen.getByRole("button", { name: "Map" }));
    expect(screen.getByRole("button", { name: `Inspect ${target.qualified_name}` }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("group", { name: "Interactive code relationship map" }).querySelectorAll("[data-graph-node]")).toHaveLength(MAP_NODE_LIMIT);
    expect(screen.getByTestId("graph-camera").getAttribute("transform")).toBe(camera);
  });

  it("includes a selected hidden symbol when returning from List to Map", () => {
    render(<KnowledgeGraph page={page([node("a"), node("b"), node("alone")], [edge("a", "b")])} />);
    fireEvent.click(screen.getByRole("button", { name: "Pause physics" }));
    expect(screen.queryByRole("button", { name: "Inspect app::alone" })).toBeNull();
    expect(screen.getByRole("button", { name: /Other symbols/ }).getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(screen.getByRole("button", { name: "List" }));
    fireEvent.click(within(screen.getByRole("list", { name: "Loaded code symbols" })).getByRole("button", { name: /app::alone/ }));
    fireEvent.click(screen.getByRole("button", { name: "Map" }));
    expect(screen.getByRole("button", { name: "Inspect app::alone" }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("button", { name: /Other symbols/ }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getAllByTestId("graph-edge")).toHaveLength(1);
  });

  it("clears selection with Escape while a symbol itself has keyboard focus", () => {
    render(<KnowledgeGraph page={page()} />);
    const symbol = screen.getByRole("button", { name: "Inspect app::work" });
    fireEvent.click(symbol);
    act(() => symbol.focus());
    expect(document.activeElement).toBe(symbol);
    expect(symbol.getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("button", { name: "Close symbol details" })).toBeTruthy();
    fireEvent.keyDown(symbol, { key: "Escape" });
    expect(symbol.getAttribute("aria-pressed")).toBe("false");
    expect(screen.queryByRole("button", { name: "Close symbol details" })).toBeNull();
  });
});
