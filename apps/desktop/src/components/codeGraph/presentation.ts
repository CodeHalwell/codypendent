import type { PositionedNode } from "./layout.js";
import { shortName } from "./layout.js";

export type Camera = { x: number; y: number; zoom: number };
export type Viewport = { width: number; height: number };
export type NodeLabel = { title: string; subtitle: string; width: number; height: number };
type Rectangle = { left: number; top: number; right: number; bottom: number };

const glyphWidth = (character: string) => /[\u2e80-\uffff]/.test(character) ? 12
  : /[MW@#%&]/.test(character) ? 10 : /[ilI.,:;!'|]/.test(character) ? 4 : 7;
const textWidth = (value: string) => [...value].reduce((width, character) => width + glyphWidth(character), 0);
function cardTitle(value: string) {
  if (textWidth(value) <= 138) return value;
  let title = "", width = 0;
  for (const character of value) {
    if (width + glyphWidth(character) > 126) break;
    title += character; width += glyphWidth(character);
  }
  return title + "…";
}

export function symbolTitle(name: string): string {
  const short = shortName(name), parts = short.split(".");
  return parts.length > 2 && !short.endsWith(".py") ? parts.slice(-2).join(".") : short;
}

export function symbolKind(node: { detail?: { kind: string } }): string {
  const kind = node.detail?.kind;
  return kind === "external_dependency" || !kind ? "Reference" : kind.charAt(0).toUpperCase() + kind.slice(1);
}

export function nodeColor(node: { detail?: { kind: string } }): string {
  const kind = node.detail?.kind ?? "";
  return ({ file: "var(--cody-link)", function: "var(--cody-teal)", method: "var(--cody-teal)",
    type: "var(--cody-purple-text)", class: "var(--cody-purple-text)", interface: "var(--cody-purple-text)",
    module: "var(--cody-success)" } as Record<string, string>)[kind] ?? "var(--cody-text-muted)";
}

export function graphLabels(nodes: PositionedNode[], viewport: Viewport, camera: Camera,
  selected: string | null, hovered: string | null): Map<string, NodeLabel> {
  const labels = new Map<string, NodeLabel>(), occupied: Rectangle[] = [];
  const budget = Math.min(36, Math.round((nodes.length < 35 ? 16 : nodes.length < 90 ? 13 : 10) * Math.sqrt(camera.zoom)));
  const priority = (node: PositionedNode) => (node.id === selected ? 10000 : node.id === hovered ? 9000 : 0)
    + (node.detail && node.detail.kind !== "external_dependency" ? 90 : 0)
    + (node.detail?.kind === "file" ? 25 : 0) + Math.min(node.degree, 20) * 8;
  for (const node of [...nodes].sort((a, b) => priority(b) - priority(a) || a.id.localeCompare(b.id))) {
    if (labels.size >= budget && node.id !== selected && node.id !== hovered) continue;
    const title = cardTitle(symbolTitle(node.name));
    const width = Math.min(184, Math.max(126, textWidth(title) + 46)), height = 42;
    const x = camera.x + node.x * camera.zoom, y = camera.y + node.y * camera.zoom;
    const box = { left: x - width / 2 - 8, right: x + width / 2 + 8, top: y - height / 2 - 8, bottom: y + height / 2 + 8 };
    if (box.left < 3 || box.right > viewport.width - 3 || box.top < 3 || box.bottom > viewport.height - 3) continue;
    if (occupied.some((other) => box.left < other.right && box.right > other.left && box.top < other.bottom && box.bottom > other.top)) continue;
    const location = node.detail?.location;
    const subtitle = location ? (location.source_path.split("/").pop() ?? location.source_path) + ":" + location.line : symbolKind(node);
    labels.set(node.id, { title, subtitle, width, height }); occupied.push(box);
  }
  return labels;
}

export function canvasPoint(clientX: number, clientY: number,
  rect: { left: number; top: number; width: number; height: number }, viewport: Viewport, camera: Camera) {
  const scale = Math.min(rect.width / viewport.width, rect.height / viewport.height) || 1;
  return {
    x: ((clientX - rect.left - (rect.width - viewport.width * scale) / 2) / scale - camera.x) / camera.zoom,
    y: ((clientY - rect.top - (rect.height - viewport.height * scale) / 2) / scale - camera.y) / camera.zoom,
  };
}

export function fitCamera(nodes: PositionedNode[], viewport: Viewport): Camera {
  if (!nodes.length) return { x: 0, y: 0, zoom: 1 };
  const left = Math.min(...nodes.map((node) => node.x)), right = Math.max(...nodes.map((node) => node.x));
  const top = Math.min(...nodes.map((node) => node.y)), bottom = Math.max(...nodes.map((node) => node.y));
  const zoom = Math.max(0.15, Math.min(1, (viewport.width - 190) / Math.max(1, right - left), (viewport.height - 110) / Math.max(1, bottom - top)));
  return { x: viewport.width / 2 - (left + right) / 2 * zoom, y: viewport.height / 2 - (top + bottom) / 2 * zoom, zoom };
}

export function connectionPath(from: PositionedNode, to: PositionedNode, labels: Map<string, NodeLabel>, zoom: number, lane = 0): string {
  const boundary = (node: PositionedNode, dx: number, dy: number) => {
    const label = labels.get(node.id), length = Math.max(1, Math.hypot(dx, dy));
    const scale = label ? 1 / Math.max(Math.abs(dx) / (label.width / 2 + 5), Math.abs(dy) / (label.height / 2 + 5), 0.001)
      : (7 + Math.min(4, Math.sqrt(node.degree))) / length;
    return { x: node.x + dx * scale / zoom, y: node.y + dy * scale / zoom };
  };
  if (from.id === to.id) {
    const half = (labels.get(from.id)?.width ?? 24) / (2 * zoom), rise = 55 / zoom + lane;
    return "M " + (from.x - half / 2) + " " + (from.y - 22 / zoom) + " C " + (from.x - half - 35 / zoom) + " " + (from.y - rise) + ", "
      + (from.x + half + 35 / zoom) + " " + (from.y - rise) + ", " + (from.x + half / 2) + " " + (from.y - 22 / zoom);
  }
  const dx = to.x - from.x, dy = to.y - from.y, length = Math.max(1, Math.hypot(dx, dy));
  const a = boundary(from, dx, dy), b = boundary(to, -dx, -dy), bend = (Math.min(32, length * 0.09) + lane) / length;
  return "M " + a.x + " " + a.y + " Q " + ((from.x + to.x) / 2 - dy * bend) + " " + ((from.y + to.y) / 2 + dx * bend) + " " + b.x + " " + b.y;
}
