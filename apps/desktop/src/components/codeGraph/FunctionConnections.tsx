import { useId } from "react";
import { ArrowDownLeft, ArrowUpRight, FileCode2 } from "lucide-react";
import type { CodeGraphEdgeView, CodeGraphSourceLocation } from "@codypendent/protocol";
import type { GraphNode } from "./layout.js";
import { symbolTitle } from "./presentation.js";
import { functionConnections, sourceLocationLabel, type FunctionConnection } from "./functionRelations.js";
import "./FunctionConnections.css";

export interface FunctionConnectionsProps {
  active: GraphNode;
  edges: CodeGraphEdgeView[];
  byId: ReadonlyMap<string, GraphNode>;
  onSelect: (id: string) => void;
  /** The inspector owns native source-open failures and displays them beside these sections. */
  onOpen?: (location: CodeGraphSourceLocation) => Promise<void>;
  loading?: boolean;
  partial?: boolean;
  error?: string;
}

export function FunctionConnections({ active, edges, byId, onSelect, onOpen, loading = false, partial = false, error }: FunctionConnectionsProps) {
  const id = useId();
  const { definition, calledBy, calls } = functionConnections(active, edges, byId);
  const incomplete = partial || Boolean(error);
  const external = active.detail?.kind === "external_dependency";
  return <section className="function-connections" aria-label="Function connections" aria-busy={loading}>
    <section className="function-section" aria-labelledby={`${id}-definition`}>
      <h4 id={`${id}-definition`}><FileCode2 size={13} aria-hidden="true" /> Defined in</h4>
      {definition ? <SourceLocation location={definition} onOpen={onOpen} action="Open definition" />
        : active.detail?.source_path ? <><p className="function-source-path">{active.detail.source_path}</p><p className="function-note">Line information is not recorded. Update the code map to add it.</p></>
        : <p className="function-note">{external ? "External reference · no definition recorded in this repository." : "Definition not loaded for this symbol."}</p>}
    </section>
    {loading && <p className="function-note" role="status">Finding callers and calls…</p>}
    {error && <div className="function-load-error" role="alert"><p>Some function connections could not be loaded.</p><details><summary>Connection details</summary><p>{error}</p></details></div>}
    {partial && !loading && <p className="function-note">Showing the loaded part of this function’s connections.</p>}
    <CallSection title="Called by" incoming entries={calledBy} activeId={active.id} loading={loading} incomplete={incomplete} onSelect={onSelect} onOpen={onOpen} />
    <CallSection title="Calls" entries={calls} activeId={active.id} loading={loading} incomplete={incomplete} onSelect={onSelect} onOpen={onOpen} />
  </section>;
}

function CallSection({ title, incoming = false, entries, activeId, loading, incomplete, onSelect, onOpen }: {
  title: string; incoming?: boolean; entries: FunctionConnection[]; activeId: string; loading: boolean; incomplete: boolean;
  onSelect: (id: string) => void; onOpen?: FunctionConnectionsProps["onOpen"];
}) {
  const id = useId();
  return <section className="function-section" aria-labelledby={id}>
    <h4 id={id}>{incoming ? <ArrowDownLeft size={13} aria-hidden="true" /> : <ArrowUpRight size={13} aria-hidden="true" />}{title}
      {entries.length > 0 && <span className="function-count" aria-label={`${entries.length} functions loaded`}>{entries.length}</span>}</h4>
    {entries.length === 0 ? <p className="function-note">{loading ? (incoming ? "Looking for callers…" : "Looking for calls…")
      : incomplete ? (incoming ? "No callers in the loaded results." : "No calls in the loaded results.")
      : incoming ? "No recorded callers." : "No recorded calls."}</p>
      : <ul className="function-call-list">{entries.map((entry) => <li key={entry.id}>
        <button type="button" className="function-target" title={entry.name} onClick={() => onSelect(entry.id)}>
          <span>{symbolTitle(entry.name)}</span><ArrowUpRight size={12} aria-hidden="true" />
        </button>
        {entry.id === activeId ? <p className="function-reference-note">This function · recursive call</p>
          : entry.node?.detail?.kind === "external_dependency" ? <p className="function-reference-note">External reference · no local definition recorded</p>
          : !entry.node?.detail && <p className="function-reference-note">Definition not loaded</p>}
        {entry.locations.length > 0 && <><p className="function-location-label">{entry.locations.length === 1 ? "Recorded call location" : "Recorded call locations"}</p>
          <ul className="function-location-list">{entry.locations.map((location) => <li key={`${location.source_path}:${location.line}:${location.column}`}>
            <SourceLocation location={location} onOpen={onOpen} action="Open call" />
          </li>)}</ul></>}
        {entry.missingLocation && <p className="function-note">Call location not recorded.</p>}
      </li>)}</ul>}
  </section>;
}

function SourceLocation({ location, onOpen, action }: { location: CodeGraphSourceLocation; onOpen?: FunctionConnectionsProps["onOpen"]; action: string }) {
  const label = sourceLocationLabel(location);
  return onOpen ? <button type="button" className="function-source-link" title={label} aria-label={`${action} at ${label}`}
    onClick={() => { void onOpen(location).catch(() => undefined); }}><span>{label}</span><ArrowUpRight size={11} aria-hidden="true" /></button>
    : <p className="function-source-path">{label}</p>;
}
