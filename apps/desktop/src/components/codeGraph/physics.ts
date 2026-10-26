import { edgeKey, MAP_HEIGHT, MAP_WIDTH, type GraphData, type PositionedNode } from "./layout.js";

export type Viewport = { width: number; height: number };
export type Particle = { node: PositionedNode; vx: number; vy: number; anchorX: number; anchorY: number };
export type PhysicsState = { particles: Map<string, Particle>; links: { from: string; to: string; length: number }[]; viewport: Viewport; alpha: number; dragging: string | null; ticks: number };
const DEFAULT_VIEWPORT = { width: MAP_WIDTH, height: MAP_HEIGHT };
const clamp = (value: number, low: number, high: number) => Math.max(low, Math.min(high, value));

/** Values, not React object identities, determine whether the graph changed. */
export function physicsSignature(data: GraphData, seeds: PositionedNode[], viewport: Viewport): string {
  return JSON.stringify([
    viewport.width, viewport.height,
    seeds.map((node) => [node.id, node.name, node.detail, node.componentId, Math.round(node.x * 100), Math.round(node.y * 100)]).sort((a, b) => String(a[0]).localeCompare(String(b[0]))),
    data.edges.map(edgeKey).sort(),
  ]);
}

export function createPhysics(data: GraphData, seeds: PositionedNode[], viewport: Viewport = DEFAULT_VIEWPORT, previous?: PhysicsState): PhysicsState {
  const particles = new Map<string, Particle>();
  const bounds = { width: Math.max(160, viewport.width), height: Math.max(140, viewport.height) };
  const resized = previous && (previous.viewport.width !== bounds.width || previous.viewport.height !== bounds.height);
  const resizeScale = resized ? Math.min(1, (bounds.width - 48) / (previous.viewport.width - 48), (bounds.height - 56) / (previous.viewport.height - 56)) : 1;
  const position = (node: PositionedNode) => resized ? {
    x: clamp(bounds.width / 2 + (node.x - previous.viewport.width / 2) * resizeScale, 24, bounds.width - 24),
    y: clamp(bounds.height / 2 + (node.y - previous.viewport.height / 2) * resizeScale, 28, bounds.height - 28),
  } : { x: node.x, y: node.y };
  const boundedSeeds = seeds.slice(0, 240);
  for (const seed of boundedSeeds) {
    const old = previous?.particles.get(seed.id);
    if (old) {
      particles.set(seed.id, { ...old, node: { ...seed, ...position(old.node) }, vx: old.vx * resizeScale, vy: old.vy * resizeScale, anchorX: resized ? seed.x : old.anchorX, anchorY: resized ? seed.y : old.anchorY });
      continue;
    }
    const neighbors = previous ? data.edges.flatMap((edge) => edge.from_id === seed.id ? [edge.to_id] : edge.to_id === seed.id ? [edge.from_id] : []).flatMap((id) => previous.particles.has(id) ? [previous.particles.get(id)!] : []) : [];
    const phase = [...seed.id].reduce((sum, character) => sum + character.charCodeAt(0), 0) * 2.39996;
    const x = neighbors.length ? neighbors.reduce((sum, node) => sum + position(node.node).x, 0) / neighbors.length + Math.cos(phase) * 65 : seed.x;
    const y = neighbors.length ? neighbors.reduce((sum, node) => sum + position(node.node).y, 0) / neighbors.length + Math.sin(phase) * 65 : seed.y;
    particles.set(seed.id, { node: { ...seed, x, y }, vx: 0, vy: 0, anchorX: x, anchorY: y });
  }
  const seen = new Set<string>();
  const links = data.edges.flatMap((edge) => {
    if (edge.from_id === edge.to_id || !particles.has(edge.from_id) || !particles.has(edge.to_id)) return [];
    const pair = [edge.from_id, edge.to_id].sort(), key = JSON.stringify(pair);
    if (seen.has(key) || seen.size >= 400) return [];
    seen.add(key);
    const from = particles.get(pair[0])!, to = particles.get(pair[1])!;
    return [{ from: pair[0], to: pair[1], length: clamp(Math.hypot(from.anchorX - to.anchorX, from.anchorY - to.anchorY), 120, 155) }];
  }).sort((a, b) => a.from.localeCompare(b.from) || a.to.localeCompare(b.to));
  return { particles, links, viewport: bounds, alpha: 0.85, dragging: previous?.dragging && particles.has(previous.dragging) ? previous.dragging : null, ticks: 0 };
}

export const physicsNodes = (state: PhysicsState): PositionedNode[] => [...state.particles.values()].map((particle) => ({ ...particle.node }));
export const physicsRunning = (state: PhysicsState) => state.links.length > 0 && (state.alpha > 0.015 || state.dragging !== null) && state.ticks < 480;
export function reheatPhysics(state: PhysicsState) { state.alpha = Math.max(state.alpha, 0.75); state.ticks = 0; }
export function beginPhysicsDrag(state: PhysicsState, id: string) {
  const particle = state.particles.get(id); if (!particle) return;
  state.dragging = id; particle.vx = 0; particle.vy = 0; reheatPhysics(state);
}
export function movePhysicsNode(state: PhysicsState, id: string, x: number, y: number) {
  const particle = state.particles.get(id); if (!particle || !Number.isFinite(x) || !Number.isFinite(y)) return;
  particle.node.x = clamp(x, 24, state.viewport.width - 24); particle.node.y = clamp(y, 28, state.viewport.height - 28);
  particle.anchorX = particle.node.x; particle.anchorY = particle.node.y; particle.vx = 0; particle.vy = 0; reheatPhysics(state);
}
export function endPhysicsDrag(state: PhysicsState) { state.dragging = null; reheatPhysics(state); }

/** Per-component forces preserve the packed islands; isolates never repel the entire graph. */
export function stepPhysics(state: PhysicsState, elapsedMs: number): boolean {
  if (!physicsRunning(state)) return false;
  const frame = clamp(Number.isFinite(elapsedMs) ? elapsedMs / (1000 / 60) : 1, 0.1, 2);
  const nodes = [...state.particles.values()], force = nodes.map(() => ({ x: 0, y: 0 }));
  const indexes = new Map(nodes.map((particle, index) => [particle.node.id, index]));
  for (let i = 0; i < nodes.length; i++) {
    const a = nodes[i];
    if (a.node.connected) {
      force[i].x += (a.anchorX - a.node.x) * 0.012;
      force[i].y += (a.anchorY - a.node.y) * 0.012;
    }
    for (let j = i + 1; j < nodes.length; j++) {
      const b = nodes[j], local = a.node.componentId === b.node.componentId && a.node.connected && b.node.connected;
      const dx = a.node.x - b.node.x, dy = a.node.y - b.node.y;
      const squaredDistance = dx * dx + dy * dy;
      if (!local && squaredDistance >= 38 * 38) continue;
      const distance = Math.sqrt(squaredDistance);
      const x = distance > 0.001 ? dx / distance : Math.cos((i + 1) * (j + 1)), y = distance > 0.001 ? dy / distance : Math.sin((i + 1) * (j + 1));
      const personalSpace = local ? 80 : 38;
      const strength = Math.min(6, Math.max(0, personalSpace - distance) * 0.075 + (local ? 850 / (squaredDistance + 144) : 0));
      force[i].x += x * strength; force[i].y += y * strength; force[j].x -= x * strength; force[j].y -= y * strength;
    }
  }
  for (const link of state.links) {
    const from = indexes.get(link.from)!, to = indexes.get(link.to)!;
    const dx = nodes[to].node.x - nodes[from].node.x, dy = nodes[to].node.y - nodes[from].node.y;
    const distance = Math.max(1, Math.hypot(dx, dy)), spring = clamp((distance - link.length) * 0.025, -8, 8);
    force[from].x += dx / distance * spring; force[from].y += dy / distance * spring;
    force[to].x -= dx / distance * spring; force[to].y -= dy / distance * spring;
  }
  nodes.forEach((particle, index) => {
    if (particle.node.id === state.dragging) { particle.vx = 0; particle.vy = 0; return; }
    particle.vx = clamp((particle.vx + force[index].x * state.alpha * frame) * Math.pow(0.83, frame), -8, 8);
    particle.vy = clamp((particle.vy + force[index].y * state.alpha * frame) * Math.pow(0.83, frame), -8, 8);
    particle.node.x = clamp(particle.node.x + particle.vx * frame, 24, state.viewport.width - 24);
    particle.node.y = clamp(particle.node.y + particle.vy * frame, 28, state.viewport.height - 28);
  });
  state.alpha = state.dragging ? Math.max(0.3, state.alpha * 0.98) : state.alpha * Math.pow(0.965, frame);
  state.ticks++;
  return physicsRunning(state);
}
