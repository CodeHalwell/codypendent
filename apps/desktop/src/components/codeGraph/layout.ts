import { placeComponents } from "./componentLayout.js";
import type { CodeGraphEdgeView, CodeGraphNodeView, CodeGraphPage } from "@codypendent/protocol";

export const MAP_WIDTH = 1000;
export const MAP_HEIGHT = 640;
export const MAP_NODE_LIMIT = 240;
export const MAP_EDGE_LIMIT = 400;
export type GraphNode = { id: string; name: string; detail?: CodeGraphNodeView };
export type GraphData = { nodes: GraphNode[]; edges: CodeGraphEdgeView[] };
export type PositionedNode = GraphNode & { x: number; y: number; degree: number; componentId: string; componentSize: number; componentLabel?: string; connected: boolean };
export type ComponentBounds = { id: string; label?: string; connected: boolean; nodeIds: string[]; x: number; y: number; width: number; height: number };

export function componentBounds(nodes: PositionedNode[]): ComponentBounds[] {
  const groups = new Map<string, PositionedNode[]>();
  for (const node of nodes) { const group = groups.get(node.componentId) ?? []; group.push(node); groups.set(node.componentId, group); }
  return [...groups].map(([id, group]) => {
    const x = Math.min(...group.map((node) => node.x)) - 22, y = Math.min(...group.map((node) => node.y)) - 28;
    return { id, label: group[0].componentLabel, connected: group[0].connected, nodeIds: group.map((node) => node.id), x, y,
      width: Math.max(...group.map((node) => node.x)) - x + 22, height: Math.max(...group.map((node) => node.y)) - y + 22 };
  });
}

export const edgeKey = (edge: CodeGraphEdgeView) => JSON.stringify([edge.from_id, edge.to_id, edge.relation, edge.revision]);
export const relationLabel = (relation: string) => relation.replace(/[_-]+/g, " ");
export function shortName(name: string): string {
  const pieces = name.split(/::|\//);
  return pieces[pieces.length - 1] || name;
}

/** Boundary symbols use only names/IDs actually supplied by an edge. */
export function collectGraph(pages: readonly CodeGraphPage[]): GraphData {
  const nodes = new Map<string, GraphNode>();
  const edges = new Map<string, CodeGraphEdgeView>();
  for (const page of pages) {
    for (const node of [...page.nodes, ...(page.related_nodes ?? [])]) nodes.set(node.id, { id: node.id, name: node.qualified_name, detail: node });
    for (const edge of page.edges) {
      edges.set(edgeKey(edge), edge);
      if (!nodes.has(edge.from_id)) nodes.set(edge.from_id, { id: edge.from_id, name: edge.from_name });
      if (!nodes.has(edge.to_id)) nodes.set(edge.to_id, { id: edge.to_id, name: edge.to_name });
    }
  }
  return { nodes: [...nodes.values()], edges: [...edges.values()] };
}

export function filterGraph(data: GraphData, query: string, relation: string): GraphData {
  const matching = data.edges.filter((edge) => !relation || edge.relation === relation);
  const needle = query.trim().toLocaleLowerCase();
  const found = new Set(data.nodes.filter((node) => !needle || `${node.name} ${node.detail?.source_path ?? ""}`.toLocaleLowerCase().includes(needle)).map((node) => node.id));
  const edges = matching.filter((edge) => !needle || found.has(edge.from_id) || found.has(edge.to_id));
  const incident = new Set(edges.flatMap((edge) => [edge.from_id, edge.to_id]));
  return { nodes: data.nodes.filter((node) => (!relation && found.has(node.id)) || incident.has(node.id)), edges };
}

export function boundGraph(data: GraphData, selected?: string | null): GraphData {
  const degrees = new Map<string, number>();
  for (const edge of data.edges) for (const id of [edge.from_id, edge.to_id]) degrees.set(id, (degrees.get(id) ?? 0) + 1);
  const neighbors = new Set(data.edges.filter((edge) => edge.from_id === selected || edge.to_id === selected).flatMap((edge) => [edge.from_id, edge.to_id]));
  const rank = (id: string) => id === selected ? 2 : neighbors.has(id) ? 1 : 0;
  const nodes = [...data.nodes].sort((a, b) => rank(b.id) - rank(a.id) || (degrees.get(b.id) ?? 0) - (degrees.get(a.id) ?? 0) || a.name.localeCompare(b.name) || a.id.localeCompare(b.id)).slice(0, MAP_NODE_LIMIT);
  const ids = new Set(nodes.map((node) => node.id));
  const edges = data.edges.filter((edge) => ids.has(edge.from_id) && ids.has(edge.to_id))
    .sort((a, b) => Number(b.from_id === selected || b.to_id === selected) - Number(a.from_id === selected || a.to_id === selected))
    .slice(0, MAP_EDGE_LIMIT);
  return { nodes, edges };
}

/** Component packing keeps unrelated symbols out of each neighborhood's geometry. */
export function layoutGraph(data: GraphData, width = MAP_WIDTH, height = MAP_HEIGHT): PositionedNode[] {
  return placeComponents(boundGraph(data), Math.max(160, width), Math.max(140, height));
}

export function edgePath(from: PositionedNode, to: PositionedNode): string {
  if (from.id === to.id) return `M ${from.x - 6} ${from.y - 7} C ${from.x - 42} ${from.y - 65}, ${from.x + 42} ${from.y - 65}, ${from.x + 8} ${from.y - 10}`;
  const distance = Math.max(1, Math.hypot(to.x - from.x, to.y - from.y));
  const x = (to.x - from.x) / distance, y = (to.y - from.y) / distance;
  return `M ${from.x + x * 12} ${from.y + y * 12} Q ${(from.x + to.x) / 2 - y * 10} ${(from.y + to.y) / 2 + x * 10} ${to.x - x * 16} ${to.y - y * 16}`;
}
