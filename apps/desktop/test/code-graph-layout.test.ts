import { describe, expect, it } from "vitest";
import { boundGraph, collectGraph, edgePath, filterGraph, layoutGraph, MAP_EDGE_LIMIT, MAP_NODE_LIMIT } from "../src/components/codeGraph/layout.js";
import { edge, node, page } from "./code-graph-fixtures.js";

describe("recorded code relationships", () => {
  it("keeps real endpoint names without inventing metadata, then fills details from exploration", () => {
    const initial = collectGraph([page()]);
    expect(initial.nodes.find((row) => row.id === "finish")).toEqual({ id: "finish", name: "app::finish" });
    const expanded = collectGraph([page(), page([node("finish")], [edge("work", "finish", "references")])]);
    expect(expanded.nodes).toHaveLength(3);
    expect(expanded.edges).toHaveLength(2);
    expect(expanded.nodes.find((row) => row.id === "finish")?.detail?.source_path).toBe("src/finish.rs");
  });
  it("retains distinct recorded relations and filters to their actual endpoints", () => {
    const data = collectGraph([page([node("start"), node("work"), node("alone")], [edge("start", "work"), edge("start", "work", "references")])]);
    expect(data.edges).toHaveLength(2);
    const filtered = filterGraph(data, "", "references");
    expect(filtered.nodes.map((row) => row.id)).toEqual(["start", "work"]);
    expect(filtered.edges.map((row) => row.relation)).toEqual(["references"]);
    expect(filterGraph(data, "alone", "").nodes.map((row) => row.id)).toEqual(["alone"]);
  });
  it("bounds a large graph without synthesizing connections and retains the inspected symbol", () => {
    const nodes = Array.from({ length: 500 }, (_, i) => node(String(i)));
    const edges = Array.from({ length: 900 }, (_, i) => edge(String(i % 500), String((i + 1) % 500), String(i)));
    const data = collectGraph([page(nodes, edges)]), bounded = boundGraph(data, "499");
    expect(bounded.nodes).toHaveLength(MAP_NODE_LIMIT);
    expect(bounded.nodes.some((row) => row.id === "499")).toBe(true);
    expect(bounded.edges.length).toBeLessThanOrEqual(MAP_EDGE_LIMIT);
    const ids = new Set(bounded.nodes.map((row) => row.id));
    expect(bounded.edges.every((row) => edges.includes(row) && ids.has(row.from_id) && ids.has(row.to_id))).toBe(true);
    const positions = layoutGraph(bounded);
    expect(positions.every((row) => Number.isFinite(row.x) && Number.isFinite(row.y))).toBe(true);
    expect(layoutGraph(bounded)).toEqual(positions);
    expect(edgePath(positions[0], positions[0])).toContain(" C ");
  });
});

describe("component-aware map placement", () => {
  it("packs the165-symbol mixed graph into separate readable islands and a compact unlinked grid", () => {
    const nodes = [], edges = [];
    for (let group = 0; group < 5; group++) {
      const root = `file-${group}`;
      nodes.push({ ...node(root), kind: "file", source_path: `src/group-${group}.rs` });
      for (let i = 0; i < 12; i++) { const id = `${root}-${i}`; nodes.push({ ...node(id), source_path: `src/group-${group}.rs` }); edges.push(edge(root, id, "defines")); }
    }
    for (let i = 0; i < 20; i++) { nodes.push(node(`pair-${i}-a`), node(`pair-${i}-b`)); edges.push(edge(`pair-${i}-a`, `pair-${i}-b`)); }
    for (let i = 0; i < 20; i++) { nodes.push(node(`chain-${i}`)); if (i) edges.push(edge(`chain-${i - 1}`, `chain-${i}`)); }
    nodes.push(node("recursive")); edges.push(edge("recursive", "recursive"));
    for (let i = 0; i < 39; i++) nodes.push(node(`unlinked-${i}`));
    expect(nodes).toHaveLength(165); expect(edges).toHaveLength(100);
    const graph = collectGraph([page(nodes, edges)]), placed = layoutGraph(graph);
    const unlinked = placed.filter((row) => !row.connected), connected = placed.filter((row) => row.connected);
    expect(unlinked).toHaveLength(39);
    expect(new Set(unlinked.map((row) => row.componentId)).size).toBe(1);
    expect(new Set(unlinked.map((row) => row.x)).size).toBeGreaterThan(3);
    expect(new Set(unlinked.map((row) => row.y)).size).toBeGreaterThan(3);
    expect(new Set(connected.map((row) => row.componentId)).size).toBe(27);
    expect(placed.every((row) => row.x >= 24 && row.x <= 976 && row.y >= 28 && row.y <= 612)).toBe(true);
    const center = unlinked[Math.floor(unlinked.length / 2)];
    expect(unlinked.some((row) => row.x < center.x)).toBe(true);
    expect(unlinked.some((row) => row.x > center.x)).toBe(true);
    expect(layoutGraph({ nodes: [...graph.nodes].reverse(), edges: [...graph.edges].reverse() })).toEqual(placed);
    expect(graph.edges).toHaveLength(100);
  });
  it("keeps a long chain legible without fitting it into a tiny straight line", () => {
    const nodes = Array.from({ length: 160 }, (_, i) => node(String(i)));
    const edges = nodes.slice(1).map((row, i) => edge(String(i), row.id));
    const placed = layoutGraph(collectGraph([page(nodes, edges)]));
    const byId = new Map(placed.map((row) => [row.id, row]));
    const lengths = edges.map((row) => { const a = byId.get(row.from_id)!, b = byId.get(row.to_id)!; return Math.hypot(a.x - b.x, a.y - b.y); });
    expect(Math.min(...lengths)).toBeGreaterThan(20);
    expect(Math.max(...lengths)).toBeLessThan(130);
    expect(Math.max(...placed.map((row) => row.y)) - Math.min(...placed.map((row) => row.y))).toBeGreaterThan(250);
  });
  it("uses the actual viewport aspect while keeping all supplied symbols and stable IDs", () => {
    const graph = collectGraph([page()]);
    const wide = layoutGraph(graph, 1400, 500), narrow = layoutGraph(graph, 480, 700);
    expect(wide.map((row) => row.id)).toEqual(narrow.map((row) => row.id));
    expect(wide.every((row) => row.x >= 24 && row.x <= 1376 && row.y >= 28 && row.y <= 472)).toBe(true);
    expect(narrow.every((row) => row.x >= 24 && row.x <= 456 && row.y >= 28 && row.y <= 672)).toBe(true);
    expect(wide).not.toEqual(narrow);
    const single = layoutGraph(collectGraph([page([node("only")], [])]));
    expect(single[0].x).toBe(500); expect(single[0].y).toBe(320);
  });
});
