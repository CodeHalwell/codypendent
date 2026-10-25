import { useEffect, useId, useMemo, useRef, useState } from "react";
import { Box, Braces, FileCode2, Link2, Network } from "lucide-react";
import type { CodeGraphEdgeView } from "@codypendent/protocol";
import { edgeKey, relationLabel, type PositionedNode } from "./layout.js";
import { canvasPoint, connectionPath, graphLabels, nodeColor, symbolKind, type Camera, type Viewport } from "./presentation.js";

type Props = {
  nodes: PositionedNode[]; edges: CodeGraphEdgeView[]; viewport: Viewport;
  camera: Camera; setCamera: React.Dispatch<React.SetStateAction<Camera>>;
  selected: string | null; onSelect: (id: string) => void; onClear: () => void;
  onZoom: (factor: number, point?: { x: number; y: number }) => void;
  onDrag: { start: (id: string) => void; move: (id: string, x: number, y: number) => void; end: () => void };
};
type Drag = ({ kind: "pan"; x: number; y: number; camera: Camera }
  | { kind: "node"; id: string; offsetX: number; offsetY: number }) & { pointerId: number };

export function GraphCanvas({ nodes, edges, viewport, camera, setCamera, selected, onSelect, onClear, onZoom, onDrag }: Props) {
  const svg = useRef<SVGSVGElement>(null), drag = useRef<Drag | null>(null), endDrag = useRef(onDrag.end);
  const [hovered, setHovered] = useState<string | null>(null);
  endDrag.current = onDrag.end;
  const marker = "graph-arrow-" + useId().replace(/:/g, "");
  const labels = useMemo(() => graphLabels(nodes, viewport, camera, selected, hovered), [nodes, viewport, camera, selected, hovered]);
  const byId = useMemo(() => new Map(nodes.map((node) => [node.id, node])), [nodes]);
  const emphasis = hovered ?? selected;
  const related = useMemo(() => new Set(edges.filter((edge) => edge.from_id === emphasis || edge.to_id === emphasis)
    .flatMap((edge) => [edge.from_id, edge.to_id])), [edges, emphasis]);
  const lanes = useMemo(() => {
    const groups = new Map<string, CodeGraphEdgeView[]>(), result = new Map<string, number>();
    for (const edge of edges) {
      const key = JSON.stringify([edge.from_id, edge.to_id]);
      groups.set(key, [...(groups.get(key) ?? []), edge]);
    }
    for (const group of groups.values()) group.sort((a, b) => edgeKey(a).localeCompare(edgeKey(b)))
      .forEach((edge, index) => result.set(edgeKey(edge), (index - (group.length - 1) / 2) * 15));
    return result;
  }, [edges]);

  useEffect(() => () => { if (drag.current?.kind === "node") endDrag.current(); }, []);
  useEffect(() => {
    const element = svg.current;
    if (!element) return;
    const wheel = (event: WheelEvent) => {
      event.preventDefault();
      const point = canvasPoint(event.clientX, event.clientY, element.getBoundingClientRect(), viewport, { x: 0, y: 0, zoom: 1 });
      onZoom(Math.exp(-event.deltaY * 0.0015), point);
    };
    element.addEventListener("wheel", wheel, { passive: false });
    return () => element.removeEventListener("wheel", wheel);
  }, [onZoom, viewport]);
  const finishDrag = (event: React.PointerEvent<SVGSVGElement>) => {
    if (drag.current && drag.current.pointerId !== event.pointerId) return;
    const current = drag.current; drag.current = null;
    setHovered(null);
    if (current?.kind === "node") onDrag.end();
    if (event.currentTarget.hasPointerCapture?.(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId);
  };

  return <svg ref={svg} viewBox={"0 0 " + viewport.width + " " + viewport.height}
    role="group" aria-label="Interactive code relationship map" tabIndex={0}
    onKeyDown={(event) => {
      if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); setHovered(null); onClear(); }
      if (event.target !== event.currentTarget) return;
      const moves: Record<string, [number, number]> = { ArrowLeft: [40, 0], ArrowRight: [-40, 0], ArrowUp: [0, 40], ArrowDown: [0, -40] };
      if (moves[event.key]) {
        event.preventDefault(); const [x, y] = moves[event.key];
        setCamera((old) => ({ ...old, x: old.x + x, y: old.y + y }));
      }
      if (event.key === "+" || event.key === "=") { event.preventDefault(); onZoom(1.25); }
      if (event.key === "-") { event.preventDefault(); onZoom(1 / 1.25); }
    }}
    onPointerDown={(event) => {
      if (drag.current || event.button !== 0 || (event.target as Element).closest("[data-graph-node]")) return;
      drag.current = { kind: "pan", x: event.clientX, y: event.clientY, camera, pointerId: event.pointerId };
      event.currentTarget.setPointerCapture?.(event.pointerId);
    }}
    onPointerMove={(event) => {
      const current = drag.current; if (!current || current.pointerId !== event.pointerId) return;
      const rect = event.currentTarget.getBoundingClientRect();
      if (current.kind === "node") {
        const point = canvasPoint(event.clientX, event.clientY, rect, viewport, camera);
        onDrag.move(current.id, point.x + current.offsetX, point.y + current.offsetY);
      } else {
        const scale = Math.min(rect.width / viewport.width, rect.height / viewport.height) || 1;
        setCamera({ ...current.camera, x: current.camera.x + (event.clientX - current.x) / scale,
          y: current.camera.y + (event.clientY - current.y) / scale });
      }
    }}
    onPointerUp={finishDrag} onPointerCancel={finishDrag}
    onLostPointerCapture={finishDrag}>
    <title>Code relationship map</title>
    <desc>Real code symbols and their directed relationships. Drag a symbol to move its connections, drag the background to pan, and scroll to zoom. List offers a searchable alternative.</desc>
    <defs><marker id={marker} viewBox="0 0 10 10" refX="9" refY="5"
      markerWidth={5 / camera.zoom} markerHeight={5 / camera.zoom} markerUnits="userSpaceOnUse" orient="auto-start-reverse">
      <path d="M 1 1 L 9 5 L 1 9" fill="none" stroke="var(--cody-text-muted)" strokeWidth="1.4" />
    </marker></defs>
    <g transform={"translate(" + camera.x + " " + camera.y + ") scale(" + camera.zoom + ")"} data-testid="graph-camera">
      {edges.map((edge) => {
        const from = byId.get(edge.from_id), to = byId.get(edge.to_id); if (!from || !to) return null;
        const highlighted = edge.from_id === emphasis || edge.to_id === emphasis;
        return <path key={edgeKey(edge)} className={"code-map-edge" + (highlighted ? " highlighted" : "") + (emphasis && !highlighted ? " dimmed" : "")}
          d={connectionPath(from, to, labels, camera.zoom, lanes.get(edgeKey(edge)))} markerEnd={"url(#" + marker + ")"}
          vectorEffect="non-scaling-stroke" data-testid="graph-edge">
          <title>{edge.from_name + " → " + relationLabel(edge.relation) + " → " + edge.to_name}</title>
        </path>;
      })}
      {nodes.map((node, index) => {
        const label = labels.get(node.id), highlighted = node.id === emphasis || related.has(node.id);
        const clip = marker + "-label-" + index;
        const kind = node.detail?.kind, Icon = kind === "file" ? FileCode2 : kind === "module" ? Network
          : ["type", "class", "interface"].includes(kind ?? "") ? Box : kind === "external_dependency" || !kind ? Link2 : Braces;
        return <g key={node.id} role="button" tabIndex={0} data-graph-node={node.id}
          aria-label={"Inspect " + node.name} aria-pressed={node.id === selected}
          className={"code-map-node" + (node.id === selected ? " selected" : "") + (emphasis && !highlighted ? " dimmed" : "") + (!node.detail || kind === "external_dependency" ? " boundary" : "")}
          style={{ "--node-color": nodeColor(node) } as React.CSSProperties}
          transform={"translate(" + node.x + " " + node.y + ") scale(" + (1 / camera.zoom) + ")"}
          onPointerEnter={() => setHovered(node.id)} onPointerLeave={() => { if (!drag.current) setHovered(null); }}
          onFocus={() => setHovered(node.id)} onBlur={() => setHovered(null)}
          onPointerDown={(event) => {
            if (drag.current || event.button !== 0 || !svg.current) return;
            event.stopPropagation(); onSelect(node.id); onDrag.start(node.id);
            const point = canvasPoint(event.clientX, event.clientY, svg.current.getBoundingClientRect(), viewport, camera);
            drag.current = { kind: "node", id: node.id, offsetX: node.x - point.x, offsetY: node.y - point.y, pointerId: event.pointerId };
            svg.current.setPointerCapture?.(event.pointerId);
          }}
          onClick={() => onSelect(node.id)}
          onKeyDown={(event) => {
            if (event.key === "Enter" || event.key === " ") { event.preventDefault(); onSelect(node.id); }
            const moves: Record<string, [number, number]> = { ArrowLeft: [-18, 0], ArrowRight: [18, 0], ArrowUp: [0, -18], ArrowDown: [0, 18] };
            if (moves[event.key]) {
              event.preventDefault(); event.stopPropagation(); const [x, y] = moves[event.key];
              onSelect(node.id); onDrag.start(node.id); onDrag.move(node.id, node.x + x / camera.zoom, node.y + y / camera.zoom); onDrag.end();
            }
          }}>
          <title>{node.name + " · " + symbolKind(node)}</title>
          {label ? <>
            {node.id === selected && <rect className="code-map-selection" x={-label.width / 2 - 4} y={-25} width={label.width + 8} height={50} rx={12} />}
            <rect className="code-map-node-card" x={-label.width / 2} y={-21} width={label.width} height={42} rx={8} data-testid="graph-node-label" />
            <Icon className="code-map-node-icon" x={-label.width / 2 + 12} y={-8} width={16} height={16} />
            <clipPath id={clip}><rect x={-label.width / 2 + 37} y={-16} width={label.width - 44} height={32} /></clipPath>
            <text className="code-map-node-label" clipPath={"url(#" + clip + ")"} x={-label.width / 2 + 37} y={-2}>{label.title}</text>
            <text className="code-map-node-subtitle" clipPath={"url(#" + clip + ")"} x={-label.width / 2 + 37} y={12}>{label.subtitle}</text>
          </> : <>
            {node.id === selected && <circle r={17} className="code-map-selection" />}
            <circle r={15} className="code-map-node-hit" fill="transparent" />
            <circle r={4 + Math.min(4, Math.sqrt(node.degree))} className="code-map-node-dot" />
          </>}
        </g>;
      })}
    </g>
  </svg>;
}
