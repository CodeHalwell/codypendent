import type { GraphData, GraphNode, PositionedNode } from "./layout.js";

type LocalNode = GraphNode & { x: number; y: number; degree: number };
type Block = { id: string; label?: string; connected: boolean; nodes: LocalNode[]; width: number; height: number; x?: number; y?: number };
const GAP_X = 110, GAP_Y = 80, PAD = 44, GUTTER = 44;
const order = (a: GraphNode, b: GraphNode) => (a.detail?.source_path ?? "").localeCompare(b.detail?.source_path ?? "") || a.name.localeCompare(b.name) || a.id.localeCompare(b.id);

function sourceLabel(nodes: GraphNode[]): string | undefined {
  const paths = [...new Set(nodes.flatMap((node) => node.detail?.source_path ? [node.detail.source_path] : []))].sort();
  if (paths.length === 1) return paths[0];
  if (!paths.length) return undefined;
  const pieces = paths.map((path) => path.split("/").slice(0, -1));
  let length = Math.min(...pieces.map((path) => path.length));
  for (let i = 0; i < length; i++) if (pieces.some((path) => path[i] !== pieces[0][i])) { length = i; break; }
  return length ? `${pieces[0].slice(0, length).join("/")}/` : undefined;
}

function grid(nodes: GraphNode[], degrees: Map<string, number>, connected: boolean): Block {
  const columns = Math.max(1, Math.min(nodes.length, Math.ceil(Math.sqrt(nodes.length * 1.65))));
  const rows = Math.ceil(nodes.length / columns);
  return {
    id: connected ? `component:${nodes[0].id}` : "unlinked",
    label: connected ? sourceLabel(nodes) : "No loaded connections",
    connected, width: (columns - 1) * GAP_X + PAD * 2, height: (rows - 1) * GAP_Y + PAD * 2,
    nodes: nodes.map((node, i) => ({ ...node, degree: degrees.get(node.id) ?? 0, x: PAD + i % columns * GAP_X, y: PAD + Math.floor(i / columns) * GAP_Y })),
  };
}

function connectedBlock(nodes: GraphNode[], adjacent: Map<string, Set<string>>, degrees: Map<string, number>, outgoing: Map<string, number>): Block {
  const terminals = nodes.every((node) => (adjacent.get(node.id)?.size ?? 0) <= 2) ? nodes.filter((node) => (adjacent.get(node.id)?.size ?? 0) === 1) : [];
  const root = [...(terminals.length ? terminals : nodes)].sort((a, b) => {
    const score = (node: GraphNode) => ((node.detail?.kind === "file" || node.detail?.kind === "module") ? 1000 : 0) + (outgoing.get(node.id) ?? 0) * 3 + (degrees.get(node.id) ?? 0);
    return score(b) - score(a) || order(a, b);
  })[0];
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const depth = new Map([[root.id, 0]]), queue = [root.id];
  for (let i = 0; i < queue.length; i++) {
    const neighbors = [...(adjacent.get(queue[i]) ?? [])].sort((a, b) => order(byId.get(a)!, byId.get(b)!));
    for (const id of neighbors) if (!depth.has(id)) { depth.set(id, depth.get(queue[i])! + 1); queue.push(id); }
  }
  const levels: GraphNode[][] = [];
  for (const id of queue) (levels[depth.get(id)!] ??= []).push(byId.get(id)!);
  // Long chains fold into a continuous snake instead of compressing every node into a line.
  if (levels.length > Math.max(5, Math.sqrt(nodes.length) * 1.6)) {
    const block = grid(queue.map((id) => byId.get(id)!), degrees, true);
    const columns = Math.max(1, Math.ceil(Math.sqrt(nodes.length * 1.65)));
    block.nodes = block.nodes.map((node, i) => ({ ...node, x: PAD + (Math.floor(i / columns) % 2 ? columns - 1 - i % columns : i % columns) * GAP_X }));
    return block;
  }
  const ranks = new Map<string, number>();
  for (const level of levels) level.forEach((node, i) => ranks.set(node.id, i));
  for (let pass = 0; pass < 4; pass++) {
    const direction = pass % 2 ? [...levels.keys()].reverse() : [...levels.keys()];
    for (const index of direction) {
      const neighborLevel = index + (pass % 2 ? 1 : -1);
      const barycenter = (node: GraphNode) => {
        const neighbors = [...(adjacent.get(node.id) ?? [])].filter((id) => depth.get(id) === neighborLevel);
        return neighbors.length ? neighbors.reduce((sum, id) => sum + ranks.get(id)!, 0) / neighbors.length : ranks.get(node.id)!;
      };
      levels[index].sort((a, b) => barycenter(a) - barycenter(b) || order(a, b));
      levels[index].forEach((node, i) => ranks.set(node.id, i));
    }
  }
  const rowBudget = Math.max(3, Math.ceil(Math.sqrt(nodes.length / 1.5)));
  const shapes = levels.map((level) => ({ rows: Math.min(rowBudget, level.length), columns: Math.ceil(level.length / rowBudget) }));
  const height = Math.max(...shapes.map((shape) => (shape.rows - 1) * GAP_Y)) + PAD * 2;
  let x = PAD;
  const positioned: LocalNode[] = [];
  levels.forEach((level, index) => {
    const shape = shapes[index];
    level.forEach((node, i) => {
      const column = Math.floor(i / shape.rows), row = i % shape.rows;
      const rows = Math.min(shape.rows, level.length - column * shape.rows);
      positioned.push({ ...node, degree: degrees.get(node.id) ?? 0, x: x + column * GAP_X, y: height / 2 + (row - (rows - 1) / 2) * GAP_Y });
    });
    x += (shape.columns - 1) * GAP_X + GAP_X * 1.6;
  });
  return { id: `component:${[...nodes].sort((a, b) => a.id.localeCompare(b.id))[0].id}`, label: sourceLabel(nodes), connected: true, nodes: positioned, width: x - GAP_X * 1.6 + PAD, height };
}

function pack(blocks: Block[], width: number) {
  const rows: { y: number; height: number; right: number }[] = [];
  const placements: { block: Block; x: number; y: number }[] = [];
  let height = 0, usedWidth = 0;
  for (const block of blocks) {
    let row = rows.find((candidate) => candidate.height >= block.height && candidate.right + block.width <= width);
    if (!row) { row = { y: height, height: block.height, right: 0 }; rows.push(row); height += block.height + GUTTER; }
    placements.push({ block, x: row.right, y: row.y });
    row.right += block.width + GUTTER; usedWidth = Math.max(usedWidth, row.right - GUTTER);
  }
  return { placements, width: usedWidth, height: height - GUTTER };
}

/** Real components are independent layout islands; unlinked symbols occupy a compact grid. */
export function placeComponents(data: GraphData, width: number, height: number): PositionedNode[] {
  const ordered = [...data.nodes].sort(order);
  if (!ordered.length) return [];
  const adjacent = new Map(ordered.map((node) => [node.id, new Set<string>()]));
  const degrees = new Map<string, number>(), outgoing = new Map<string, number>();
  for (const edge of [...data.edges].sort((a, b) => a.from_id.localeCompare(b.from_id) || a.to_id.localeCompare(b.to_id) || a.relation.localeCompare(b.relation))) {
    if (!adjacent.has(edge.from_id) || !adjacent.has(edge.to_id)) continue;
    adjacent.get(edge.from_id)!.add(edge.to_id); adjacent.get(edge.to_id)!.add(edge.from_id);
    for (const id of [edge.from_id, edge.to_id]) degrees.set(id, (degrees.get(id) ?? 0) + 1);
    outgoing.set(edge.from_id, (outgoing.get(edge.from_id) ?? 0) + 1);
  }
  const visited = new Set<string>(), blocks: Block[] = [], isolated: GraphNode[] = [];
  const byId = new Map(ordered.map((node) => [node.id, node]));
  for (const node of ordered) {
    if (visited.has(node.id)) continue;
    if (!degrees.get(node.id)) { isolated.push(node); visited.add(node.id); continue; }
    const members = [node.id]; visited.add(node.id);
    for (let i = 0; i < members.length; i++) for (const id of [...adjacent.get(members[i])!].sort()) if (!visited.has(id)) { members.push(id); visited.add(id); }
    blocks.push(connectedBlock(members.map((id) => byId.get(id)!), adjacent, degrees, outgoing));
  }
  if (isolated.length) blocks.push(grid(isolated, degrees, false));
  blocks.sort((a, b) => b.height - a.height || b.width - a.width || a.id.localeCompare(b.id));
  const minimum = Math.max(...blocks.map((block) => block.width));
  const area = blocks.reduce((sum, block) => sum + (block.width + GUTTER) * (block.height + GUTTER), 0);
  const maximum = Math.max(minimum, Math.sqrt(area * width / height) * 1.8);
  let best = pack(blocks, minimum), score = Infinity;
  for (let trial = 0; trial < 30; trial++) {
    const candidate = pack(blocks, minimum + (maximum - minimum) * trial / 29);
    const scale = Math.max(candidate.width / (width - 80), candidate.height / (height - 80));
    const candidateScore = scale + candidate.width * candidate.height / area * 0.025;
    if (candidateScore < score) { score = candidateScore; best = candidate; }
  }
  const scale = Math.min((width - 80) / Math.max(1, best.width), (height - 80) / Math.max(1, best.height), 1.6);
  const offsetX = (width - best.width * scale) / 2, offsetY = (height - best.height * scale) / 2;
  return best.placements.flatMap(({ block, x, y }) => block.nodes.map((node) => ({
    ...node, x: offsetX + (x + node.x) * scale, y: offsetY + (y + node.y) * scale,
    componentId: block.id, componentSize: block.nodes.length, componentLabel: block.label, connected: block.connected,
  }))).sort((a, b) => a.id.localeCompare(b.id));
}
