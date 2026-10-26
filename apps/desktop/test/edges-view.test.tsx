import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { DesktopTransport } from "../src/transport.js";
import { EdgesView } from "../src/components/EdgesView.js";
import { node, page, status } from "./code-graph-fixtures.js";
const transport = (overrides: object) => {
  const input = overrides as { readCodeGraph?: (query: object) => Promise<unknown> };
  return { codeGraphStatus: vi.fn().mockResolvedValue(status), ...overrides,
    readCodeGraph: vi.fn((query: { node_id?: string }) => {
      if (!query.node_id) return input.readCodeGraph?.(query);
      const fixture = page(), edges = fixture.edges.filter((edge) => edge.from_id === query.node_id || edge.to_id === query.node_id);
      return Promise.resolve({ ...fixture, nodes: [node(query.node_id)], related_nodes: fixture.nodes.filter((row) => row.id !== query.node_id), edges, total_nodes: 1, total_edges: edges.length });
    }),
  } as unknown as DesktopTransport;
};

describe("repository code map", () => {
  it("builds an empty repository map with one action and reloads the actual result", async () => {
    const empty = page([], []), read = vi.fn().mockResolvedValueOnce(empty).mockResolvedValue(page());
    const build = vi.fn().mockResolvedValue({ files_folded: 3, cap_hit: false });
    const overview = vi.fn().mockResolvedValueOnce({ ...status, nodes: 0, edges: 0 }).mockResolvedValue(status);
    render(<EdgesView transport={transport({ readCodeGraph: read, buildCodeGraph: build, codeGraphStatus: overview })} />);
    await screen.findByText("Your code map starts here");
    await act(async () => { fireEvent.click(screen.getAllByRole("button", { name: "Build code map" })[0]); });
    expect(build).toHaveBeenCalledTimes(1);
    expect(read).toHaveBeenCalledTimes(2);
    expect(read).toHaveBeenLastCalledWith({ include_nodes: true, include_edges: true, connected_view: true, limit: 100 });
    expect(screen.getAllByTestId("graph-edge")).toHaveLength(1);
    expect(screen.getByText("Map updated from 3 files.")).toBeTruthy();
  });
  it("narrows server queries and explores an exact symbol without carrying unrelated filters", async () => {
    const read = vi.fn().mockResolvedValue(page());
    const backend = transport({ readCodeGraph: read });
    render(<EdgesView transport={backend} />);
    await screen.findByRole("group", { name: "Interactive code relationship map" });
    fireEvent.change(screen.getByLabelText("Find a symbol"), { target: { value: "  work  " } });
    fireEvent.change(screen.getByLabelText("Folder or file"), { target: { value: " src/ " } });
    fireEvent.change(screen.getByLabelText("Language"), { target: { value: "rust" } });
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Search graph" })); });
    expect(read).toHaveBeenLastCalledWith({ name: "work", path: "src/", language: "rust", include_nodes: true, include_edges: true, limit: 100 });
    fireEvent.click(screen.getByRole("button", { name: "Inspect app::work" }));
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Explore connections" })); });
    expect(backend.readCodeGraph).toHaveBeenLastCalledWith({ node_id: "work", include_nodes: true, include_edges: true, include_neighbors: true, limit: 500 });
  });
  it("keeps failed reads distinct from empty results and reports bounded pages", async () => {
    const read = vi.fn().mockRejectedValueOnce(new Error("Index database unavailable")).mockResolvedValue({ ...page(), total_nodes: 850, total_edges: 1200 });
    render(<EdgesView transport={transport({ readCodeGraph: read })} />);
    await screen.findByText("Index database unavailable");
    expect(screen.queryByText("Your code map starts here")).toBeNull();
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Try again" })); });
    expect(screen.getByText(/Showing a connected neighbourhood around app::start: 2 symbols and 2 connections/)).toBeTruthy();
    expect(read).toHaveBeenLastCalledWith({ include_nodes: true, include_edges: true, connected_view: true, limit: 100 });
  });
  it("ignores an earlier slow search after a newer search has completed", async () => {
    let resolve!: (value: ReturnType<typeof page>) => void;
    const read = vi.fn().mockResolvedValueOnce(page()).mockImplementationOnce(() => new Promise((done) => { resolve = done; })).mockResolvedValueOnce(page([node("new")], []));
    render(<EdgesView transport={transport({ readCodeGraph: read })} />);
    await screen.findByRole("group", { name: "Interactive code relationship map" });
    fireEvent.change(screen.getByLabelText("Find a symbol"), { target: { value: "old" } });
    fireEvent.click(screen.getByRole("button", { name: "Search graph" }));
    fireEvent.change(screen.getByLabelText("Find a symbol"), { target: { value: "new" } });
    fireEvent.click(screen.getByRole("button", { name: "Search graph" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Inspect app::new" })).toBeTruthy());
    await act(async () => { resolve(page([node("old")], [])); });
    expect(screen.queryByRole("button", { name: "Inspect app::old" })).toBeNull();
  });
  it("retries the failed search rather than the previous successful filters", async () => {
    const read = vi.fn().mockResolvedValueOnce(page()).mockRejectedValueOnce(new Error("Connection interrupted")).mockResolvedValueOnce(page([node("target")], []));
    render(<EdgesView transport={transport({ readCodeGraph: read })} />);
    await screen.findByRole("group", { name: "Interactive code relationship map" });
    fireEvent.change(screen.getByLabelText("Find a symbol"), { target: { value: " target " } });
    fireEvent.change(screen.getByLabelText("Folder or file"), { target: { value: " src/target.rs " } });
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Search graph" })); });
    expect(screen.getByText("Connection interrupted")).toBeTruthy();
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Try again" })); });
    expect(read).toHaveBeenCalledTimes(3);
    expect(read).toHaveBeenLastCalledWith({ name: "target", path: "src/target.rs", include_nodes: true, include_edges: true, limit: 100 });
    expect(screen.getByRole("button", { name: "Inspect app::target" })).toBeTruthy();
  });
  it("keeps active language and symbol filters visible and routed when the overview refresh fails", async () => {
    const read = vi.fn().mockResolvedValue(page());
    const overview = vi.fn().mockResolvedValueOnce(status).mockRejectedValue(new Error("Overview temporarily unavailable"));
    render(<EdgesView transport={transport({ readCodeGraph: read, codeGraphStatus: overview })} />);
    await screen.findByRole("group", { name: "Interactive code relationship map" });
    const language = screen.getByLabelText("Language") as HTMLSelectElement;
    const kind = screen.getByLabelText("Symbol type") as HTMLSelectElement;
    fireEvent.change(language, { target: { value: "rust" } });
    fireEvent.change(kind, { target: { value: "function" } });
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Search graph" })); });
    expect(screen.getByText("Repository overview is unavailable.")).toBeTruthy();
    expect(language.selectedOptions[0].textContent).toBe("rust");
    expect(kind.selectedOptions[0].textContent).toBe("function");
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Search graph" })); });
    expect(read).toHaveBeenLastCalledWith({ language: "rust", kind: "function", include_nodes: true, include_edges: true, limit: 100 });
  });
});
