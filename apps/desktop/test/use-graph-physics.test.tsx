import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { collectGraph, layoutGraph } from "../src/components/codeGraph/layout.js";
import { useGraphPhysics } from "../src/components/codeGraph/useGraphPhysics.js";
import { edge, node, page } from "./code-graph-fixtures.js";
let pending: Map<number, FrameRequestCallback>, next: number, time: number;
const fixture = () => collectGraph([page([node("a"), node("b")], [edge("a", "b")])]);
const frames = (count: number) => act(() => { for (let i = 0; i < count; i++) { const callbacks = [...pending.values()]; pending.clear(); time += 1000 / 60; for (const callback of callbacks) callback(time); } });
beforeEach(() => {
  pending = new Map(); next = 1; time = 0;
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => { const id = next++; pending.set(id, callback); return id; });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => pending.delete(id));
  vi.stubGlobal("matchMedia", () => ({ matches: false, addEventListener: vi.fn(), removeEventListener: vi.fn() }));
});
afterEach(() => vi.unstubAllGlobals());

describe("graph physics lifecycle", () => {
  it("keeps topology preparation out of animation frames and unchanged viewport renders", () => {
    const graph = fixture(); let reads = 0;
    const seeds = layoutGraph(graph).map((seed) => ({ ...seed, get name() { reads++; return seed.name; } }));
    const hook = renderHook(({ width }) => useGraphPhysics(graph, seeds, true, { width, height: 640 }), { initialProps: { width: 1000 } });
    const initialReads = reads;
    frames(10);
    hook.rerender({ width: 1000 });
    expect(reads).toBe(initialReads);
    hook.rerender({ width: 1200 });
    expect(reads).toBeGreaterThan(initialReads);
    hook.unmount();
  });
  it("does not reset on inspection renders, pauses without ticks, settles, and cancels frames on unmount", () => {
    const graph = fixture(), seeds = layoutGraph(graph);
    const hook = renderHook(({ data, initial }) => useGraphPhysics(data, initial, true), { initialProps: { data: graph, initial: seeds } });
    frames(5); const settledSoFar = hook.result.current.nodes;
    hook.rerender({ data: { nodes: [...graph.nodes], edges: [...graph.edges] }, initial: seeds.map((row) => ({ ...row })) });
    expect(hook.result.current.nodes).toEqual(settledSoFar);
    act(() => hook.result.current.setEnabled(false)); expect(pending.size).toBe(0);
    frames(5); expect(hook.result.current.nodes).toEqual(settledSoFar);
    act(() => hook.result.current.setEnabled(true)); frames(180);
    expect(hook.result.current.running).toBe(false); expect(pending.size).toBe(0);
    act(() => hook.result.current.beginDrag("a")); expect(pending.size).toBe(1);
    hook.unmount(); expect(pending.size).toBe(0);
  });
  it("defaults reduced motion to paused and permits direct dragging without moving neighbors", () => {
    vi.stubGlobal("matchMedia", () => ({ matches: true, addEventListener: vi.fn(), removeEventListener: vi.fn() }));
    const graph = fixture(), hook = renderHook(() => useGraphPhysics(graph, layoutGraph(graph), true));
    expect(hook.result.current.enabled).toBe(false); expect(pending.size).toBe(0);
    const before = hook.result.current.nodes.find((row) => row.id === "b");
    act(() => { hook.result.current.beginDrag("a"); hook.result.current.moveNode("a", 300, 200); hook.result.current.endDrag(); });
    expect(hook.result.current.nodes.find((row) => row.id === "a")?.x).toBe(300);
    expect(hook.result.current.nodes.find((row) => row.id === "b")).toEqual(before);
    expect(pending.size).toBe(0);
    act(() => hook.result.current.setEnabled(true));
    expect(hook.result.current.nodes.find((row) => row.id === "a")?.x).toBe(300);
    expect(pending.size).toBe(1);
    frames(20); expect(hook.result.current.nodes.find((row) => row.id === "b")).not.toEqual(before);
    hook.unmount();
  });
  it("retains dragged positions when real data expands and stops when the map is inactive", () => {
    const graph = fixture();
    const hook = renderHook(({ data, active }) => useGraphPhysics(data, layoutGraph(data), active), { initialProps: { data: graph, active: false } });
    expect(pending.size).toBe(0);
    act(() => { hook.result.current.beginDrag("a"); hook.result.current.moveNode("a", 350, 230); hook.result.current.endDrag(); });
    const expanded = collectGraph([page([node("a"), node("b"), node("new")], [edge("a", "b"), edge("b", "new")])]);
    hook.rerender({ data: expanded, active: false });
    expect(hook.result.current.nodes.find((row) => row.id === "a")?.x).toBe(350);
    expect(hook.result.current.nodes).toHaveLength(3);
    hook.rerender({ data: expanded, active: true }); expect(pending.size).toBe(1);
    hook.rerender({ data: expanded, active: false }); expect(pending.size).toBe(0); hook.unmount();
  });
});
