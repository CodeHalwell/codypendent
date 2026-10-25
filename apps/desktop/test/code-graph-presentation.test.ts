import { describe, expect, it } from "vitest";
import type { PositionedNode } from "../src/components/codeGraph/layout.js";
import { canvasPoint, connectionPath, fitCamera, graphLabels, type Camera, type NodeLabel } from "../src/components/codeGraph/presentation.js";
import { node } from "./code-graph-fixtures.js";

const viewport = { width: 1000, height: 640 };
const identity: Camera = { x: 0, y: 0, zoom: 1 };
const positioned = (id: string, x: number, y: number, name = `app::symbol_${id}`): PositionedNode => ({
  id, name, detail: node(id, name), x, y, degree: 2,
  componentId: "component:fixture", componentSize: 36, connected: true,
});
const numbers = (path: string) => (path.match(/-?\d*\.?\d+(?:e[-+]?\d+)?/gi) ?? []).map(Number);

describe("code graph presentation", () => {
  it.each([identity, { x: -75, y: -40, zoom: 1.6 }])("keeps crowded labels separate and inside the visible viewport at $zoom zoom", (camera) => {
    const nodes = Array.from({ length: 36 }, (_, i) => positioned(String(i).padStart(2, "0"), 110 + i % 6 * 140, 75 + Math.floor(i / 6) * 95));
    nodes.push(positioned("outside-left", -100, 250), positioned("outside-right", 1200, 250));
    const labels = graphLabels(nodes, viewport, camera, null, null);
    expect(labels.size).toBeGreaterThan(2);
    expect(labels.size).toBeLessThan(nodes.length);
    expect(labels.has("outside-left")).toBe(false);
    expect(labels.has("outside-right")).toBe(false);
    const boxes = [...labels].map(([id, label]) => {
      const point = nodes.find((entry) => entry.id === id)!;
      const x = camera.x + point.x * camera.zoom, y = camera.y + point.y * camera.zoom;
      return { left: x - label.width / 2 - 8, right: x + label.width / 2 + 8, top: y - label.height / 2 - 8, bottom: y + label.height / 2 + 8 };
    });
    for (let i = 0; i < boxes.length; i++) {
      const box = boxes[i];
      expect(box.left).toBeGreaterThanOrEqual(0);
      expect(box.right).toBeLessThanOrEqual(viewport.width);
      expect(box.top).toBeGreaterThanOrEqual(0);
      expect(box.bottom).toBeLessThanOrEqual(viewport.height);
      for (const other of boxes.slice(i + 1)) {
        expect(box.right <= other.left || box.left >= other.right || box.bottom <= other.top || box.top >= other.bottom).toBe(true);
      }
    }
  });

  it("gives the selected symbol priority over a crowded high-degree neighbor and hovered label", () => {
    const crowded = Array.from({ length: 30 }, (_, i) => ({ ...positioned(String(i), 300, 250), degree: 20 }));
    const selected = { ...positioned("selected", 300, 250), degree: 0, detail: undefined };
    const hovered = positioned("hovered", 300, 250);
    const labels = graphLabels([...crowded, selected, hovered], viewport, identity, selected.id, hovered.id);
    expect([...labels.keys()]).toEqual([selected.id]);
    expect(graphLabels([...crowded, selected, { ...hovered, x: 700 }], viewport, identity, selected.id, hovered.id).has(hovered.id)).toBe(true);
  });

  it("ellipsizes wide and CJK titles within bounded label cards", () => {
    const names = ["i".repeat(40), "W".repeat(40), "資料検索処理".repeat(8)];
    const nodes = names.map((name, i) => positioned(String(i), 180 + i * 320, 300, `app::${name}`));
    const labels = graphLabels(nodes, viewport, identity, null, null);
    expect(labels.size).toBe(3);
    const [narrow, wide, cjk] = nodes.map((entry) => labels.get(entry.id)!);
    for (const [i, label] of [narrow, wide, cjk].entries()) {
      expect(label.title.endsWith("…")).toBe(true);
      expect(label.title.length).toBeLessThan(names[i].length);
      expect(label.width).toBeGreaterThan(0);
      expect(label.width).toBeLessThanOrEqual(184);
      expect(Number.isFinite(label.height)).toBe(true);
    }
    expect(wide.title.length).toBeLessThan(narrow.title.length);
    expect(cjk.title.length).toBeLessThan(narrow.title.length);
  });

  it.each([
    { left: 31, top: 57, width: 1400, height: 600 },
    { left: 80, top: 100, width: 480, height: 900 },
  ])("converts a scaled, letterboxed pointer through the zoomed camera ($width×$height)", (rect) => {
    const camera = { x: 37, y: -26, zoom: 2.5 }, world = { x: 225, y: 330 };
    const scale = Math.min(rect.width / viewport.width, rect.height / viewport.height);
    const clientX = rect.left + (rect.width - viewport.width * scale) / 2 + (camera.x + world.x * camera.zoom) * scale;
    const clientY = rect.top + (rect.height - viewport.height * scale) / 2 + (camera.y + world.y * camera.zoom) * scale;
    const point = canvasPoint(clientX, clientY, rect, viewport, camera);
    expect(point.x).toBeCloseTo(world.x, 10);
    expect(point.y).toBeCloseTo(world.y, 10);
    const moved = canvasPoint(clientX + 48, clientY - 24, rect, viewport, camera);
    expect(moved.x - point.x).toBeCloseTo(48 / (scale * camera.zoom), 10);
    expect(moved.y - point.y).toBeCloseTo(-24 / (scale * camera.zoom), 10);
  });

  it.each([0.5, 1, 2.5])("keeps directed curves finite and separates parallel lanes at %s zoom", (zoom) => {
    const from = positioned("from", 180, 200), to = positioned("to", 760, 430);
    const labels = new Map<string, NodeLabel>([[from.id, { title: "From", subtitle: "Function", width: 160, height: 42 }], [to.id, { title: "To", subtitle: "Function", width: 150, height: 42 }]]);
    const base = connectionPath(from, to, labels, zoom), parallel = connectionPath(from, to, labels, zoom, 22);
    expect(base).toContain(" Q ");
    expect(parallel).not.toBe(base);
    const a = numbers(base), b = numbers(parallel);
    expect(a).toHaveLength(6);
    expect(a.slice(0, 2)).toEqual(b.slice(0, 2));
    expect(a.slice(-2)).toEqual(b.slice(-2));
    expect(a.slice(2, 4)).not.toEqual(b.slice(2, 4));
    const loop = connectionPath(from, from, labels, zoom), secondLoop = connectionPath(from, from, labels, zoom, 22);
    expect(loop).toContain(" C ");
    expect(secondLoop).not.toBe(loop);
    for (const path of [base, parallel, loop, secondLoop, connectionPath(to, from, new Map(), zoom), connectionPath(from, { ...to, x: from.x, y: from.y }, labels, zoom)]) {
      expect(path).not.toMatch(/NaN|Infinity|undefined/);
      expect(numbers(path).every(Number.isFinite)).toBe(true);
    }
  });

  it.each([{ width: 1000, height: 640 }, { width: 480, height: 400 }])("fits all node cards within a $width×$height viewport", (area) => {
    const nodes = [positioned("a", 24, 28), positioned("b", 976, 612), positioned("c", 100, 550)];
    const camera = fitCamera(nodes, area);
    expect(Number.isFinite(camera.x) && Number.isFinite(camera.y) && Number.isFinite(camera.zoom)).toBe(true);
    expect(camera.zoom).toBeGreaterThan(0);
    expect(camera.zoom).toBeLessThanOrEqual(1);
    for (const entry of nodes) {
      const x = camera.x + entry.x * camera.zoom, y = camera.y + entry.y * camera.zoom;
      expect(x - 92).toBeGreaterThanOrEqual(0);
      expect(x + 92).toBeLessThanOrEqual(area.width);
      expect(y - 21).toBeGreaterThanOrEqual(0);
      expect(y + 21).toBeLessThanOrEqual(area.height);
    }
    expect(fitCamera([], area)).toEqual(identity);
    const single = fitCamera([positioned("only", 225, 330)], area);
    expect(single.x + 225 * single.zoom).toBe(area.width / 2);
    expect(single.y + 330 * single.zoom).toBe(area.height / 2);
  });
});
