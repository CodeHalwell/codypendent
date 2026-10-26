import type { CodeGraphPage } from "@codypendent/protocol";
import { layoutGraph, type GraphData, type GraphNode } from "./layout.js";

export const callable = (node: GraphNode) => ["function", "method", "test"].includes(node.detail?.kind ?? "");
export function entryFunction(page: CodeGraphPage): string | null {
  const score = (id: string) => page.edges.reduce((sum, edge) => sum + (edge.relation === "calls"
    ? Number(edge.to_id === id) * 4 + Number(edge.from_id === id) : 0), 0);
  return [...page.nodes].filter((node) => ["function", "method", "test"].includes(node.kind))
    .sort((a, b) => score(b.id) - score(a.id) || a.qualified_name.localeCompare(b.qualified_name))[0]?.id ?? null;
}
export function functionNeighborhood(data: GraphData, selected: string | null): GraphData {
  const edges = data.edges.filter((edge) => edge.relation === "calls" && (!selected || edge.from_id === selected || edge.to_id === selected));
  const ids = new Set(edges.flatMap((edge) => [edge.from_id, edge.to_id]));
  if (selected) ids.add(selected);
  return { edges, nodes: data.nodes.filter((node) => ids.has(node.id)) };
}
export function layoutCalls(data: GraphData, selected: string, width: number, height: number) {
  const incoming = new Set(data.edges.filter((edge) => edge.to_id === selected).map((edge) => edge.from_id));
  const rows = (side: boolean) => data.nodes.filter((node) => node.id !== selected && incoming.has(node.id) === side).sort((a, b) => a.name.localeCompare(b.name));
  const left = rows(true), right = rows(false);
  return layoutGraph(data, width, height).map((node) => {
    if (node.id === selected) return { ...node, x: width / 2, y: height / 2 };
    const list = incoming.has(node.id) ? left : right, index = list.findIndex((item) => item.id === node.id);
    return { ...node, x: width * (incoming.has(node.id) ? 0.22 : 0.78), y: 58 + (height - 116) * (index + 0.5) / Math.max(1, list.length) };
  });
}
