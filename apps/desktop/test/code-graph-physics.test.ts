import { describe, expect, it } from "vitest";
import { collectGraph, layoutGraph } from "../src/components/codeGraph/layout.js";
import { beginPhysicsDrag, createPhysics, endPhysicsDrag, movePhysicsNode, physicsNodes, physicsRunning, stepPhysics } from "../src/components/codeGraph/physics.js";
import { edge, node, page } from "./code-graph-fixtures.js";
const setup = () => { const graph = collectGraph([page([node("a"), node("b"), node("unlinked")], [edge("a", "b")])]); return { graph, seeds: layoutGraph(graph) }; };

describe("bounded code-map physics", () => {
  it("holds the dragged symbol, pulls its connected neighbor, and leaves unlinked symbols in place", () => {
    const { graph, seeds } = setup(), state = createPhysics(graph, seeds);
    const before = physicsNodes(state), a = state.particles.get("a")!, oldB = state.particles.get("b")!.node.x;
    beginPhysicsDrag(state, "a"); movePhysicsNode(state, "a", a.node.x + 120, a.node.y + 70);
    const held = { ...a.node };
    for (let i = 0; i < 20; i++) stepPhysics(state, 1000 / 60);
    expect(a.node).toEqual(held); expect(a.vx).toBe(0); expect(a.vy).toBe(0);
    expect(state.particles.get("b")!.node.x).not.toBe(oldB);
    expect(state.particles.get("unlinked")!.node).toEqual(before.find((row) => row.id === "unlinked"));
    endPhysicsDrag(state);
    for (let i = 0; i < 200; i++) stepPhysics(state, 1000 / 60);
    expect(physicsRunning(state)).toBe(false);
  });
  it("clamps a hidden-tab frame and remains finite at the240-node400-edge bound", () => {
    const { graph, seeds } = setup(), normal = createPhysics(graph, seeds), delayed = createPhysics(graph, seeds);
    stepPhysics(normal, 1000 / 30); stepPhysics(delayed, 120_000);
    expect(physicsNodes(delayed)).toEqual(physicsNodes(normal));
    const nodes = Array.from({ length: 240 }, (_, i) => node(String(i))), edges = Array.from({ length: 400 }, (_, i) => edge(String(i % 240), String((i * 7 + 1) % 240)));
    const large = collectGraph([page(nodes, edges)]), state = createPhysics(large, layoutGraph(large));
    for (let i = 0; i < 180; i++) stepPhysics(state, 1000 / 60);
    expect(physicsNodes(state)).toHaveLength(240);
    expect(state.links.length).toBeLessThanOrEqual(400);
    expect(physicsNodes(state).every((row) => Number.isFinite(row.x) && Number.isFinite(row.y) && row.x >= 24 && row.x <= 976 && row.y >= 28 && row.y <= 612)).toBe(true);
  });
  it("preserves positions on expansion, prunes removed IDs, and changes anchors on resize", () => {
    const { graph, seeds } = setup(), state = createPhysics(graph, seeds);
    movePhysicsNode(state, "a", 420, 280);
    const expanded = collectGraph([page([node("a"), node("b"), node("new")], [edge("a", "b"), edge("b", "new"), edge("new", "missing")])]);
    const newSeeds = layoutGraph(expanded).filter((row) => row.id !== "missing");
    const next = createPhysics(expanded, newSeeds, { width: 1000, height: 640 }, state);
    expect(next.particles.get("a")!.node.x).toBe(420);
    expect(next.particles.has("unlinked")).toBe(false); expect(next.particles.has("missing")).toBe(false);
    expect(next.links).toHaveLength(2);
    const wideSeeds = layoutGraph(expanded, 2186, 640).filter((row) => row.id !== "missing");
    const resized = createPhysics(expanded, wideSeeds, { width: 2186, height: 640 }, next);
    expect(resized.particles.get("a")!.node.x).toBe(1013);
    expect(resized.particles.get("a")!.node.y).toBe(280);
    expect(resized.particles.get("b")!.node.x - resized.particles.get("a")!.node.x).toBeCloseTo(next.particles.get("b")!.node.x - 420);
    expect(resized.particles.get("a")!.anchorX).toBe(wideSeeds.find((row) => row.id === "a")!.x);
  });
  it("scales a shrinking viewport uniformly around its center while keeping nodes in bounds", () => {
    const { graph, seeds } = setup(), state = createPhysics(graph, seeds);
    movePhysicsNode(state, "a", 900, 550);
    const resized = createPhysics(graph, layoutGraph(graph, 480, 320), { width: 480, height: 320 }, state);
    const scale = Math.min(432 / 952, 264 / 584);
    expect(resized.particles.get("a")!.node.x).toBeCloseTo(240 + 400 * scale);
    expect(resized.particles.get("a")!.node.y).toBeCloseTo(160 + 230 * scale);
    expect(physicsNodes(resized).every((row) => row.x >= 24 && row.x <= 456 && row.y >= 28 && row.y <= 292)).toBe(true);
  });
});
