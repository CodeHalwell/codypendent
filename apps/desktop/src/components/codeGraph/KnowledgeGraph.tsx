import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { CircleDot, Expand, List, Map as MapIcon, Minus, Network, Pause, Play, Plus, RotateCcw } from "lucide-react";
import type { CodeGraphPage } from "@codypendent/protocol";
import { boundGraph, collectGraph, filterGraph, layoutGraph, MAP_HEIGHT, MAP_WIDTH, relationLabel } from "./layout.js";
import { GraphCanvas } from "./GraphCanvas.js";
import { GraphGuide, GraphInspector } from "./GraphInspector.js";
import { fitCamera, nodeColor, symbolKind, type Camera, type Viewport } from "./presentation.js";
import { useGraphPhysics } from "./useGraphPhysics.js";
import { useSymbolDetails } from "./useSymbolDetails.js";
import { entryFunction, functionNeighborhood, layoutCalls } from "./callGraph.js";
import "./codeGraph.css";

const INITIAL: Camera = { x: 0, y: 0, zoom: 1 };

export function KnowledgeGraph({ page, readNeighbors, defaultMode = "structure", onOpenFile }: {
  page: CodeGraphPage; readNeighbors?: (id: string) => Promise<CodeGraphPage>;
  defaultMode?: "calls" | "structure"; onOpenFile?: (file: string, line?: number) => Promise<void>;
}) {
  const [pages, setPages] = useState<CodeGraphPage[]>([]);
  const [expanded, setExpanded] = useState<string[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [focused, setFocused] = useState<string | null>(null);
  const [relation, setRelation] = useState("");
  const [view, setView] = useState<"map" | "list">("map");
  const [mode, setMode] = useState(defaultMode);
  const [camera, setCamera] = useState<Camera>(INITIAL);
  const [includeOther, setIncludeOther] = useState(false);
  const [loading, setLoading] = useState<string | null>(null);
  const [failure, setFailure] = useState<{ id: string; detail: string } | null>(null);
  const [listQuery, setListQuery] = useState("");
  const [viewport, setViewport] = useState<Viewport>({ width: MAP_WIDTH, height: MAP_HEIGHT });
  const epoch = useRef(0), stage = useRef<HTMLDivElement>(null);

  useEffect(() => {
    epoch.current++;
    setPages([]); setExpanded([]); setSelected(defaultMode === "calls" ? entryFunction(page) : null); setFocused(null); setRelation(""); setMode(defaultMode);
    setLoading(null); setFailure(null); setCamera(INITIAL); setIncludeOther(false);
    return () => { epoch.current++; };
  }, [page, defaultMode]);
  useEffect(() => {
    const element = stage.current;
    if (!element || typeof ResizeObserver === "undefined") return;
    const measure = () => {
      const rect = element.getBoundingClientRect();
      if (rect.width <= 0 || rect.height <= 0) return;
      const width = Math.round(rect.width), height = Math.round(rect.height);
      setViewport((old) => old.width === width && old.height === height ? old : { width, height });
    };
    measure();
    const observer = new ResizeObserver(measure); observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const details = useSymbolDetails(page, defaultMode === "calls" || mode === "calls" ? selected : null, readNeighbors);
  const all = useMemo(() => collectGraph([page, ...pages, ...details.pages]), [page, pages, details.pages]);
  const relations = useMemo(() => [...new Set(all.edges.map((edge) => edge.relation))].sort(), [all]);
  const filtered = useMemo(() => {
    const data = mode === "calls" ? functionNeighborhood(all, selected) : filterGraph(all, "", relation);
    if (!focused) return data;
    const edges = data.edges.filter((edge) => edge.from_id === focused || edge.to_id === focused);
    const ids = new Set([focused, ...edges.flatMap((edge) => [edge.from_id, edge.to_id])]);
    return { edges, nodes: data.nodes.filter((node) => ids.has(node.id)) };
  }, [all, relation, focused, mode, selected]);
  const linked = useMemo(() => new Set(filtered.edges.flatMap((edge) => [edge.from_id, edge.to_id])), [filtered]);
  const otherCount = filtered.nodes.filter((node) => !linked.has(node.id)).length;
  const hideOther = !includeOther && filtered.edges.length > 0 && otherCount > 0;
  const mapData = useMemo(() => hideOther
    ? { edges: filtered.edges, nodes: filtered.nodes.filter((node) => linked.has(node.id)) }
    : filtered, [filtered, hideOther, linked]);
  const base = useMemo(() => boundGraph(mapData), [mapData]);
  const bounded = useMemo(() => selected && mapData.nodes.some((node) => node.id === selected)
    && !base.nodes.some((node) => node.id === selected) ? boundGraph(mapData, selected) : base, [base, mapData, selected]);
  const wide = viewport.width > 960;
  const mapWidth = mode === "calls" && selected && wide ? viewport.width - 300 : viewport.width;
  const seeds = useMemo(() => mode === "calls" && selected ? layoutCalls(bounded, selected, mapWidth, viewport.height)
    : layoutGraph(bounded, viewport.width, viewport.height), [bounded, viewport, mode, selected, mapWidth]);
  const physics = useGraphPhysics(bounded, seeds, view === "map", viewport);
  useEffect(() => { if (mode === "calls" && selected) { physics.reset(); setCamera(INITIAL); } }, [mode, selected, wide, physics.reset]);
  const active = all.nodes.find((node) => node.id === selected);
  const allActiveEdges = all.edges.filter((edge) => edge.from_id === selected || edge.to_id === selected);
  const byId = useMemo(() => new Map(all.nodes.map((node) => [node.id, node])), [all]);
  const listed = all.nodes.filter((node) => (node.name + " " + (node.detail?.source_path ?? "")).toLowerCase().includes(listQuery.trim().toLowerCase()));
  const fileCount = new Set(all.nodes.flatMap((node) => node.detail?.source_path ? [node.detail.source_path] : [])).size;
  useEffect(() => { setCamera(INITIAL); }, [relation, focused, mode]);

  const zoom = useCallback((factor: number, point = { x: mapWidth / 2, y: viewport.height / 2 }) => setCamera((old) => {
    const next = Math.max(0.15, Math.min(5, old.zoom * factor));
    return { zoom: next, x: point.x - (point.x - old.x) * next / old.zoom, y: point.y - (point.y - old.y) * next / old.zoom };
  }), [viewport, mapWidth]);
  const selectNode = (id: string) => { if (!linked.has(id)) setIncludeOther(true); setSelected(id); };
  const expand = async () => {
    if (!selected || !readNeighbors || loading || expanded.includes(selected) || pages.length >= 6) return;
    const id = selected, current = epoch.current;
    setLoading(id); setFailure(null);
    try {
      const answer = await readNeighbors(id);
      if (epoch.current !== current) return;
      setPages((old) => [...old, answer]); setExpanded((old) => [...old, id]);
      if (answer.total_edges > answer.edges.length) setFailure({ id, detail: "Loaded " + answer.edges.length + " of " + answer.total_edges.toLocaleString() + " connections for this symbol. Narrow the repository search to explore the rest." });
    } catch (error) {
      if (epoch.current === current) setFailure({ id, detail: error instanceof Error ? error.message : String(error) });
    } finally { if (epoch.current === current) setLoading(null); }
  };

  return <section className="knowledge-graph" aria-label="Code knowledge graph">
    <div className="code-map-toolbar">
      <div className="code-map-title"><Network size={19} /><div><strong>{mode === "calls" ? "Function explorer" : "Knowledge map"}</strong><small>{mode === "calls" ? "Callers → definition → calls" : "Files, types and dependencies"}</small></div></div>
      <div className="code-map-switch" aria-label="Relationship view">
        <button type="button" aria-pressed={mode === "calls"} onClick={() => { setMode("calls"); setFocused(null); if (!selected) setSelected(entryFunction(page)); }}>Calls</button>
        <button type="button" aria-pressed={mode === "structure"} onClick={() => { setMode("structure"); setFocused(null); }}>Structure</button>
      </div>
      <div className="code-map-switch" aria-label="Graph display">
        <button type="button" aria-pressed={view === "map"} onClick={() => setView("map")}><MapIcon size={14} /> Map</button>
        <button type="button" aria-pressed={view === "list"} onClick={() => setView("list")}><List size={14} /> List</button>
      </div>
      {mode === "structure" && <label className="code-map-relation">Connections <select aria-label="Filter connections" value={relation}
        onChange={(event) => { setRelation(event.target.value); setFocused(null); }}><option value="">All types</option>
        {relations.map((name) => <option key={name} value={name}>{relationLabel(name)}</option>)}</select></label>}
      {focused && <button type="button" onClick={() => setFocused(null)}>Show all symbols</button>}
      {view === "map" && <>
        {otherCount > 0 && filtered.edges.length > 0 && <button className="code-map-other-toggle" type="button" aria-pressed={includeOther}
          title="Include symbols with no loaded connections. Every loaded symbol is also available in List."
          onClick={() => setIncludeOther((old) => !old)}>Other symbols · {otherCount}</button>}
        <button className="code-map-physics" type="button" aria-label={physics.enabled ? "Pause physics" : "Resume physics"}
          aria-pressed={physics.enabled} data-running={physics.running} onClick={() => physics.setEnabled((old) => !old)}>
          {physics.enabled ? <Pause size={13} /> : <Play size={13} />}{physics.enabled ? "Physics on" : "Physics paused"}</button>
        <div className="code-map-zoom">
          <button type="button" aria-label="Zoom out" onClick={() => zoom(1 / 1.25)}><Minus size={15} /></button>
          <span>{Math.round(camera.zoom * 100)}%</span>
          <button type="button" aria-label="Zoom in" onClick={() => zoom(1.25)}><Plus size={15} /></button>
          <button type="button" onClick={() => setCamera(fitCamera(physics.nodes, { ...viewport, width: mapWidth }))}><Expand size={14} /> Fit</button>
        </div>
      </>}
    </div>
    <div className={"code-map-workspace" + (active ? " has-selection" : "")}>
      <div className="code-map-stage" ref={stage}>
        {view === "map" ? <>
          <GraphCanvas nodes={physics.nodes} edges={bounded.edges} viewport={viewport} camera={camera} setCamera={setCamera}
            selected={selected} onSelect={selectNode} onClear={() => setSelected(null)} onZoom={zoom}
            onDrag={{ start: physics.beginDrag, move: physics.moveNode, end: physics.endDrag }} />
          {mode === "calls" && selected && <div className="code-map-call-lanes" style={{ right: wide ? 300 : 0 }} aria-hidden="true"><span>Called by</span><span>Selected symbol</span><span>Calls</span></div>}
          <div className="code-map-canvas-tools"><button type="button" aria-label="Reset layout" title="Reset the arrangement"
            onClick={() => { physics.reset(); setCamera(INITIAL); }}><RotateCcw size={14} /></button></div>
          {bounded.nodes.length === 0 && <div className="code-map-overlay">No symbols match this connection filter.</div>}
          <div className="code-map-hint">Drag a symbol to rearrange · Drag the canvas to pan · Scroll to zoom</div>
        </> : <div className="code-map-list">
          <label>Find in these results<input aria-label="Find loaded symbols" value={listQuery} onChange={(event) => setListQuery(event.target.value)} placeholder="Symbol or file name" /></label>
          <ul aria-label="Loaded code symbols">{listed.slice(0, 500).map((node) => <li key={node.id}>
            <button type="button" aria-pressed={node.id === selected} onClick={() => selectNode(node.id)}>
              <CircleDot size={16} style={{ color: nodeColor(node) }} /><span><strong>{node.name}</strong>
                <small>{symbolKind(node) + (node.detail?.language ? " · " + node.detail.language : "") + (node.detail?.source_path ? " · " + node.detail.source_path : "")}</small></span>
            </button></li>)}</ul>
          {listed.length === 0 && <p>No loaded symbols match your search.</p>}
          {listed.length > 500 && <p>Showing 500 of {listed.length} loaded symbols. Refine this search to find another symbol.</p>}
        </div>}
      </div>
      <GraphInspector active={active} edges={allActiveEdges} allEdges={allActiveEdges} byId={byId} onOpenFile={onOpenFile}
        detailsLoading={details.loading} detailsPartial={Boolean(details.page && details.page.total_edges > details.page.edges.length)} detailsError={details.error}
        onRetryDetails={details.retry}
        onClose={() => setSelected(null)} onSelect={selectNode} onFocus={() => { if (active) { setFocused(active.id); setView("map"); } }}
        onExplore={readNeighbors ? () => void expand() : undefined} loading={Boolean(loading)} expanded={Boolean(active && expanded.includes(active.id))}
        atLimit={pages.length >= 6} failure={failure?.id === active?.id ? failure?.detail : undefined} />
      <GraphGuide symbolCount={all.nodes.length} fileCount={fileCount} />
    </div>
    <div className="code-map-footer" role="status">
      <span>{view === "map" ? bounded.nodes.length : filtered.nodes.length} symbols · {view === "map" ? bounded.edges.length : filtered.edges.length} connections in view</span>
      <span>{view === "map" ? physics.running ? "Settling" : physics.enabled ? "Ready to explore" : "Physics paused" : "Arrows show recorded relationships"}
        {view === "map" && hideOther ? " · " + otherCount + " other symbols in List" : ""}
        {view === "map" && (bounded.nodes.length < mapData.nodes.length || bounded.edges.length < mapData.edges.length) ? " · Map limited for readability; use List or narrow your search" : ""}
        {pages.length > 0 ? " · " + pages.length + " neighbourhoods explored" : ""}</span>
    </div>
  </section>;
}
