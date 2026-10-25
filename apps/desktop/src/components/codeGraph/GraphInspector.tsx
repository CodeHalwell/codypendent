import { useRef, useState } from "react";
import { Focus, GitBranch, Hand, MousePointer2, Search, X } from "lucide-react";
import type { CodeGraphEdgeView, CodeGraphSourceLocation } from "@codypendent/protocol";
import { edgeKey, relationLabel, type GraphNode } from "./layout.js";
import { symbolKind, symbolTitle } from "./presentation.js";
import { FunctionConnections } from "./FunctionConnections.js";

type Props = {
  active?: GraphNode; edges: CodeGraphEdgeView[];
  onClose: () => void; onSelect: (id: string) => void; onFocus: () => void;
  onExplore?: () => void; loading: boolean; expanded: boolean; atLimit: boolean; failure?: string;
  allEdges?: CodeGraphEdgeView[]; byId: ReadonlyMap<string, GraphNode>;
  onOpenFile?: (file: string, line?: number) => Promise<void>;
  detailsLoading?: boolean; detailsPartial?: boolean; detailsError?: string; onRetryDetails: () => void;
};

export function GraphInspector({ active, edges, onClose, onSelect,
  onFocus, onExplore, loading, expanded, atLimit, failure, allEdges = edges, byId, onOpenFile,
  detailsLoading, detailsPartial, detailsError, onRetryDetails }: Props) {
  const [sourceError, setSourceError] = useState<{ id: string; detail: string } | null>(null);
  const sourceRequest = useRef({ id: active?.id, generation: 0 });
  if (sourceRequest.current.id !== active?.id) sourceRequest.current = { id: active?.id, generation: 0 };
  const openSource = async (location: CodeGraphSourceLocation) => {
    const current = sourceRequest.current, generation = ++current.generation;
    setSourceError(null);
    try { await onOpenFile?.(location.source_path, location.line); }
    catch (error) {
      if (sourceRequest.current === current && current.generation === generation && current.id) {
        setSourceError({ id: current.id, detail: error instanceof Error ? error.message : String(error) });
      }
    }
  };
  if (!active) return null;
  return <aside className="code-map-inspector" aria-label="Symbol details">
      <div className="code-map-inspector-heading"><span>In focus</span><button type="button" aria-label="Close symbol details" onClick={onClose}><X size={15} /></button></div>
      <div className="code-map-selection-header"><GitBranch size={17} /><div><h3>{symbolTitle(active.name)}</h3><small>{symbolKind(active)}{active.detail?.language ? " · " + active.detail.language : ""}</small></div></div>
      {symbolTitle(active.name) !== active.name && active.name !== active.detail?.source_path && <p className="code-map-qualified">{active.name}</p>}
      {!active.detail && <p className="code-map-muted">Load its connections to see source details.</p>}
      <FunctionConnections active={active} edges={allEdges} byId={byId} onSelect={onSelect}
        onOpen={onOpenFile ? openSource : undefined} loading={detailsLoading} partial={detailsPartial} error={detailsError} />
      {detailsError && <button type="button" onClick={onRetryDetails}>Retry connections</button>}
      {sourceError?.id === active.id && <p role="alert" className="code-map-warning">Could not open source: {sourceError.detail}</p>}
      <div className="code-map-inspector-actions">
        <button type="button" onClick={onFocus}><Focus size={14} /> Focus</button>
        {onExplore && <button type="button" disabled={loading || expanded || atLimit} onClick={onExplore}>{loading ? "Loading…" : expanded ? "Connections loaded" : "Explore connections"}</button>}
      </div>
      {atLimit && <p className="code-map-muted">This map has reached its exploration limit. Search the repository to start a new view.</p>}
      {failure && <p role="status" className="code-map-warning">{failure}</p>}
      <details className="code-map-technical" open={["file", "type", "class", "interface", "module"].includes(active.detail?.kind ?? "")}><summary>Related code · {edges.length}</summary>
      <ul className="code-map-connections">{edges.slice(0, 100).map((edge) => {
        const outgoing = edge.from_id === active.id, target = outgoing ? edge.to_id : edge.from_id, name = outgoing ? edge.to_name : edge.from_name;
        return <li key={edgeKey(edge)}><button type="button" onClick={() => onSelect(target)}>
          <small>{(outgoing ? "→ " : "← ") + relationLabel(edge.relation)}</small><span>{symbolTitle(name)}</span>
        </button><details><summary>Evidence</summary><p>{edge.evidence_kind + " · confidence " + edge.confidence.toFixed(2)}</p>
          {edge.asserted_by && <p>{edge.asserted_by.rationale}</p>}<p className="code-map-muted">Revision {edge.revision}</p>
        </details></li>;
      })}</ul>
      {edges.length > 100 && <p className="code-map-muted">Showing 100 of {edges.length} loaded connections. Use the connection filter to narrow this view.</p>}
      {edges.length === 0 && <p className="code-map-muted">No connections are loaded for this symbol in this view.</p>}
      </details>
      <details className="code-map-technical"><summary>Symbol details</summary><dl><dt>ID</dt><dd>{active.id}</dd>
        {active.detail && <><dt>Revision</dt><dd>{active.detail.revision}</dd>{active.detail.package && <><dt>Package</dt><dd>{active.detail.package}</dd></>}</>}
      </dl></details>
  </aside>;
}

/** Remains mounted beside the inspector so selection cannot shrink the page. */
export function GraphGuide({ symbolCount, fileCount }: { symbolCount: number; fileCount: number }) {
  return <aside className="code-map-guide-panel" aria-label="Map guide">
    <div className="code-map-inspector-empty">
      <GitBranch size={23} /><h3>A little context, a clearer picture</h3>
      <p>Choose a symbol to see how it connects. Drag one to move its neighbourhood.</p>
      <div className="code-map-legend">
        <span><i style={{ background: "var(--cody-link)" }} /> Files</span>
        <span><i style={{ background: "var(--cody-teal)" }} /> Functions</span>
        <span><i style={{ background: "var(--cody-purple-text)" }} /> Types</span>
        <span><i className="boundary" /> References</span>
      </div>
      <div className="code-map-overview"><span><strong>{symbolCount}</strong> loaded symbols</span><span><strong>{fileCount}</strong> known source files</span></div>
      <ul className="code-map-guide"><li><MousePointer2 /> Select to inspect</li><li><Hand /> Drag to rearrange</li><li><Search /> Search to follow a particular symbol</li></ul>
    </div>
  </aside>;
}
