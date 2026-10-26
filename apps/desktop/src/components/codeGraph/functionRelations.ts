import type { CodeGraphEdgeView, CodeGraphSourceLocation } from "@codypendent/protocol";
import type { GraphNode } from "./layout.js";

export interface FunctionConnection {
  id: string;
  name: string;
  node?: GraphNode;
  locations: CodeGraphSourceLocation[];
  missingLocation: boolean;
}

export function hasSourceLocation(location?: CodeGraphSourceLocation): location is CodeGraphSourceLocation {
  return Boolean(location?.source_path.trim() && Number.isInteger(location.line) && location.line > 0
    && Number.isInteger(location.column) && location.column > 0);
}

export function sourceLocationLabel(location: CodeGraphSourceLocation): string {
  return `${location.source_path}:${location.line}${location.column > 1 ? `:${location.column}` : ""}`;
}

const locationKey = (location: CodeGraphSourceLocation) => JSON.stringify([location.source_path, location.line, location.column]);

/** One row per function; only locations actually present on recorded call edges. */
export function functionConnections(active: GraphNode, edges: readonly CodeGraphEdgeView[], byId: ReadonlyMap<string, GraphNode>) {
  const group = (incoming: boolean): FunctionConnection[] => {
    const groups = new Map<string, FunctionConnection>();
    for (const edge of edges) {
      if (edge.relation !== "calls" || (incoming ? edge.to_id : edge.from_id) !== active.id) continue;
      const id = incoming ? edge.from_id : edge.to_id;
      const node = byId.get(id);
      let entry = groups.get(id);
      if (!entry) {
        entry = { id, node, name: node?.name ?? (incoming ? edge.from_name : edge.to_name), locations: [], missingLocation: false };
        groups.set(id, entry);
      }
      const location = edge.call_site;
      if (hasSourceLocation(location)) {
        if (!entry.locations.some((recorded) => locationKey(recorded) === locationKey(location))) entry.locations.push(location);
      } else entry.missingLocation = true;
    }
    for (const entry of groups.values()) entry.locations.sort((a, b) => a.source_path.localeCompare(b.source_path) || a.line - b.line || a.column - b.column);
    return [...groups.values()].sort((a, b) => a.name.localeCompare(b.name) || a.id.localeCompare(b.id));
  };
  return { definition: hasSourceLocation(active.detail?.location) ? active.detail.location : undefined,
    calledBy: group(true), calls: group(false) };
}
