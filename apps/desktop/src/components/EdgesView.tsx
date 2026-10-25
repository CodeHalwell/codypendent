import React, { useCallback, useEffect, useRef, useState } from "react";
import { GitBranch, RefreshCw, Search } from "lucide-react";
import type { CodeGraphPage, CodeGraphQuery, CodeGraphScanReport, CodeGraphStatusView } from "@codypendent/protocol";
import type { DesktopTransport } from "../transport.js";
import { EmptyState, PageShell } from "./PageShell.js";
import { GraphPlot } from "./GraphPlot.js";
import { KnowledgeGraph } from "./codeGraph/KnowledgeGraph.js";
import { surfaceButton, surfaceStyles as s } from "./surfaceChrome.js";

export interface EdgesViewProps { transport: DesktopTransport | null; unavailable?: string | null; onOpenFile?: (file: string, line?: number) => Promise<void> }
const PAGE_SIZE = 100;
type Read<T> = { status: "idle" | "loading" } | { status: "loaded"; value: T } | { status: "failed"; detail: string };
interface Filters { name: string; path: string; language: string; kind: string }
const EMPTY_FILTERS: Filters = { name: "", path: "", language: "", kind: "" };
const describe = (error: unknown) => error instanceof Error ? error.message : String(error);
export function buildQuery(filters: Filters, limit: number): CodeGraphQuery {
  const query: CodeGraphQuery = { include_nodes: true, include_edges: true, limit };
  for (const key of ["name", "path", "language", "kind"] as const) if (filters[key].trim()) query[key] = filters[key].trim();
  if (!Object.values(filters).some((value) => value.trim())) query.connected_view = true;
  return query;
}
export function isCut(shown: number, total: number): boolean { return shown < total; }

export const EdgesView: React.FC<EdgesViewProps> = ({ transport, unavailable, onOpenFile }) => {
  const [filters, setFilters] = useState<Filters>(EMPTY_FILTERS);
  const [answered, setAnswered] = useState<Filters>(EMPTY_FILTERS);
  const [page, setPage] = useState<Read<CodeGraphPage>>({ status: "idle" });
  const [status, setStatus] = useState<Read<CodeGraphStatusView>>({ status: "idle" });
  const [filterOptions, setFilterOptions] = useState<{ languages: string[]; kinds: string[] }>({ languages: [], kinds: [] });
  const [building, setBuilding] = useState(false);
  const [buildError, setBuildError] = useState<string | null>(null);
  const [report, setReport] = useState<CodeGraphScanReport | null>(null);
  const pageEpoch = useRef(0), statusEpoch = useRef(0), scope = useRef(0), buildPending = useRef(false);
  const attempted = useRef<Filters>(EMPTY_FILTERS);
  const canRead = !!transport?.readCodeGraph;
  const load = useCallback(async (next: Filters) => {
    if (!transport?.readCodeGraph) return;
    attempted.current = next;
    const current = ++pageEpoch.current;
    setPage({ status: "loading" });
    try {
      const value = await transport.readCodeGraph(buildQuery(next, PAGE_SIZE));
      if (current === pageEpoch.current) { setPage({ status: "loaded", value }); setAnswered(next); }
    } catch (error) { if (current === pageEpoch.current) setPage({ status: "failed", detail: describe(error) }); }
  }, [transport]);
  const loadStatus = useCallback(async () => {
    if (!transport?.codeGraphStatus) return;
    const current = ++statusEpoch.current;
    setStatus({ status: "loading" });
    try {
      const value = await transport.codeGraphStatus();
      if (current === statusEpoch.current) {
        setFilterOptions({ languages: value.by_language.map((row) => row.language), kinds: value.by_kind.map((row) => row.label) });
        setStatus({ status: "loaded", value });
      }
    }
    catch (error) { if (current === statusEpoch.current) setStatus({ status: "failed", detail: describe(error) }); }
  }, [transport]);
  useEffect(() => {
    scope.current++; buildPending.current = false;
    setFilters(EMPTY_FILTERS); setAnswered(EMPTY_FILTERS); setReport(null); setBuildError(null); setBuilding(false);
    setPage({ status: "idle" }); setStatus({ status: "idle" });
    setFilterOptions({ languages: [], kinds: [] });
    void load(EMPTY_FILTERS); void loadStatus();
    return () => { pageEpoch.current++; statusEpoch.current++; scope.current++; };
  }, [load, loadStatus]);
  const refresh = () => { void load(answered); void loadStatus(); };
  const retry = () => { void load(attempted.current); void loadStatus(); };
  const clear = () => { setFilters(EMPTY_FILTERS); void load(EMPTY_FILTERS); };
  const build = async () => {
    if (!transport?.buildCodeGraph || buildPending.current) return;
    const current = scope.current; buildPending.current = true; setBuilding(true); setBuildError(null);
    try {
      const value = await transport.buildCodeGraph();
      if (current !== scope.current) return;
      setReport(value); await Promise.all([load(filters), loadStatus()]);
    } catch (error) { if (current === scope.current) setBuildError(describe(error)); }
    finally { if (current === scope.current) { buildPending.current = false; setBuilding(false); } }
  };
  const neighbors = useCallback((id: string) => {
    if (!transport?.readCodeGraph) return Promise.reject(new Error("The workspace is unavailable."));
    return transport.readCodeGraph({ node_id: id, include_nodes: true, include_edges: true, include_neighbors: true, limit: 500 });
  }, [transport]);
  const empty = status.status === "loaded" && status.value.nodes === 0 && status.value.edges === 0;
  const filtered = Object.values(answered).some((value) => value.trim());
  const update = (key: keyof Filters, value: string) => setFilters((old) => ({ ...old, [key]: value }));
  const buildAction = transport?.buildCodeGraph && <button type="button" style={surfaceButton("primary")} disabled={building} onClick={() => void build()}><GitBranch size={14} /> {building ? "Building map…" : empty ? "Build code map" : "Update map"}</button>;

  return <PageShell style={s.page}>
    <header style={s.header}>
      <div><h1 style={s.title}>Code graph</h1><p style={s.subtitle}>Find where code is defined, who calls it, and how the pieces depend on each other.</p></div>
      <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
        <button type="button" style={surfaceButton()} onClick={refresh} disabled={!canRead || building || page.status === "loading"}><RefreshCw size={14} /> Refresh</button>{buildAction}
      </div>
    </header>
    <div style={s.scroll}>
    {!canRead ? <EmptyState state="unavailable" title="Code graph is unavailable" description={unavailable ?? "Connect your workspace to explore its code."} /> : <>
      <form className="code-graph-query" onSubmit={(event) => { event.preventDefault(); if (!building) { void load(filters); void loadStatus(); } }}>
        <label>Find a symbol<input placeholder="Function, method, class or model…" value={filters.name} onChange={(event) => update("name", event.target.value)} /></label>
        <label>Folder or file<input placeholder="Anywhere in this repository" value={filters.path} onChange={(event) => update("path", event.target.value)} /></label>
        <button type="submit" style={surfaceButton()} disabled={building}><Search size={14} /> Search graph</button>
        <details className="code-graph-filters"><summary>Language and symbol filters</summary><div>
          <label>Language<select value={filters.language} onChange={(event) => update("language", event.target.value)}><option value="">All languages</option>{[...new Set([...filterOptions.languages, ...(filters.language ? [filters.language] : [])])].map((language) => <option key={language} value={language}>{language}</option>)}</select></label>
          <label>Symbol type<select value={filters.kind} onChange={(event) => update("kind", event.target.value)}><option value="">All types</option>{[...new Set([...filterOptions.kinds, ...(filters.kind ? [filters.kind] : [])])].map((kind) => <option key={kind} value={kind}>{kind}</option>)}</select></label>
        </div></details>
      </form>
      {status.status === "loaded" && <div className="code-graph-summary" aria-label="Repository graph totals"><span><strong>{status.value.nodes.toLocaleString()}</strong> symbols</span><span><strong>{status.value.edges.toLocaleString()}</strong> connections</span><span><strong>{status.value.files.toLocaleString()}</strong> files</span></div>}
      {building && <p className="code-graph-notice" role="status">Building your repository’s code map. You can leave this page while it finishes.</p>}
      {buildError && <p className="code-graph-notice code-graph-warning" role="alert">Could not build the code map: {buildError}</p>}
      {report && !building && <p className="code-graph-notice" role="status">Map updated from {report.files_folded.toLocaleString()} files.{report.cap_hit && " This scan reached its file limit; the map covers part of this repository."}</p>}
      {status.status === "loaded" && status.value.stale && <div className="code-graph-notice code-graph-warning">This map needs an update to match your working copy.{status.value.stale_reason && <details><summary>What changed</summary>{status.value.stale_reason}</details>}</div>}
      {status.status === "failed" && <div className="code-graph-notice code-graph-warning">Repository overview is unavailable.<details><summary>Details</summary>{status.detail}</details></div>}
      {(page.status === "idle" || page.status === "loading") && <EmptyState state="loading" title="Loading your code map…" description="Reading symbols and their relationships." />}
      {page.status === "failed" && <EmptyState state="unavailable" title="Could not load the code map" description={page.detail} action={<button type="button" style={surfaceButton()} onClick={retry}>Try again</button>} />}
      {page.status === "loaded" && (page.value.nodes.length === 0 && page.value.edges.length === 0 ? <EmptyState title={filtered ? "No matching symbols" : "Your code map starts here"} description={filtered ? "Try a shorter name or search another folder." : "Build a map of this repository to explore its symbols and connections."} action={filtered ? <button type="button" style={surfaceButton()} onClick={clear}>Clear filters</button> : buildAction} /> : <>
        {!filtered && page.value.edges.length > 0 ? <p className="code-graph-notice">Showing a connected neighbourhood around {page.value.nodes[0]?.qualified_name ?? "your code"}: {page.value.nodes.length.toLocaleString()} symbols and {page.value.edges.length.toLocaleString()} connections. Search to explore another part of the repository.</p>
          : (isCut(page.value.nodes.length, page.value.total_nodes) || isCut(page.value.edges.length, page.value.total_edges)) && <p className="code-graph-notice">Loaded {page.value.nodes.length.toLocaleString()} of {page.value.total_nodes.toLocaleString()} matching symbols and {page.value.edges.length.toLocaleString()} of {page.value.total_edges.toLocaleString()} connections. Search to narrow the map, or select a symbol to explore its connections.</p>}
        <KnowledgeGraph page={page.value} readNeighbors={neighbors} defaultMode="calls" onOpenFile={onOpenFile} />
      </>)}
      {status.status === "loaded" && <details className="code-graph-overview"><summary>Repository overview and scan details</summary><div><GraphPlot status={status.value} /><p style={{ overflowWrap: "anywhere" }}>{status.value.repository_root}</p>{status.value.head_revision && <p>Revision: {status.value.head_revision}</p>}{report?.not_folded && <p>{report.not_folded.reduce((total, row) => total + row.files, 0)} files could not be included in the latest scan.</p>}</div></details>}
    </>}
    </div>
  </PageShell>;
};
