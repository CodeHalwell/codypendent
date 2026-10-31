import { useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import { AlertTriangle, GitBranch, Play, RefreshCw, Search } from "lucide-react";
import type { WorkflowCatalog } from "@codypendent/protocol";
import type { DesktopTransport } from "../transport.js";
import { MAX_WORKFLOW_INPUT_BYTES, bindWorkflowLaunchMemory, createWorkflowLaunchMemory, newWorkflowRequestKey, parseWorkflowInputs, preferWorkflowRun, rememberWorkflowRun, workflowInputEntries, workflowIntent, type WorkflowCatalogEntry, type WorkflowLaunchMemory } from "./workflowInputs.js";
import "./WorkflowLauncher.css";

export interface WorkflowLauncherProps {
  transport: DesktopTransport | null;
  repository?: string | null;
  connectionEpoch?: number;
  unavailable?: string | null;
  launchMemory?: WorkflowLaunchMemory;
  onStarted: (runId: string) => void;
  onOpenRepository?: () => void;
}

type Boundary = { transport: DesktopTransport | null; repository: string | null; epoch: number };
type CatalogState =
  | { boundary: Boundary; status: "idle" }
  | { boundary: Boundary; status: "loading" }
  | { boundary: Boundary; status: "loaded"; value: WorkflowCatalog }
  | { boundary: Boundary; status: "failed"; detail: string };
type EditorState = {
  boundary: Boundary;
  query: string;
  selected: string | null;
  drafts: Map<string, Map<string, string>>;
  errors: Map<string, string>;
  pending: boolean;
  feedback: { type: "error" | "success"; text: string } | null;
};

const emptyEditor = (boundary: Boundary, memory: WorkflowLaunchMemory): EditorState => {
  const saved = memory.editors.get(boundary.repository ?? "");
  return { boundary, query: saved?.query ?? "", selected: saved?.selected ?? null, drafts: saved?.drafts ?? new Map(), errors: new Map(), pending: false, feedback: null };
};
const describe = (error: unknown) => error instanceof Error ? error.message : String(error);
const origin = (entry: WorkflowCatalogEntry) => ({ BuiltIn: "Built-in", User: "User", Repository: "Repository", Unknown: "Unknown source" })[entry.source.type] ?? "Unknown source";

/** Catalog discovery prepares inputs; only the explicit Start action submits work. */
export function WorkflowLauncher({ transport, repository = null, connectionEpoch = 0, unavailable = null, launchMemory, onStarted, onOpenRepository }: WorkflowLauncherProps) {
  const [localMemory] = useState(createWorkflowLaunchMemory);
  const memory = launchMemory ?? localMemory;
  bindWorkflowLaunchMemory(memory, transport);
  const boundary = useMemo<Boundary>(() => ({ transport, repository, epoch: connectionEpoch }), [transport, repository, connectionEpoch, memory]);
  const currentBoundary = useRef(boundary);
  currentBoundary.current = boundary;
  const mounted = useRef(false);
  const availability = useRef({ detail: unavailable, generation: 0 });
  if (availability.current.detail !== unavailable) availability.current = { detail: unavailable, generation: availability.current.generation + 1 };
  const observedAvailability = useRef({ boundary, unavailable });
  const [savedEditor, setEditor] = useState(() => emptyEditor(boundary, memory));
  const editor = savedEditor.boundary === boundary ? savedEditor : emptyEditor(boundary, memory);
  const liveEditor = useRef(editor); liveEditor.current = editor;
  const [savedCatalog, setCatalog] = useState<CatalogState>({ boundary, status: "idle" });
  const catalog = savedCatalog.boundary === boundary ? savedCatalog : { boundary, status: "idle" as const };
  const reads = useRef(0);
  const pending = useRef<{ boundary: Boundary; key: string } | null>(null);
  const unresolved = memory.keys;
  const latestStarted = useRef(onStarted);
  latestStarted.current = onStarted;
  const fieldRefs = useRef(new Map<string, HTMLTextAreaElement>());
  const feedbackRef = useRef<HTMLParagraphElement>(null);
  const focusError = useRef<{ type: "field"; name: string } | { type: "form" } | null>(null);
  const prefix = useId();

  const updateEditor = useCallback((update: (state: EditorState) => EditorState, remember = false) => {
    if (currentBoundary.current !== boundary) return;
    const next = update(liveEditor.current.boundary === boundary ? liveEditor.current : emptyEditor(boundary, memory));
    liveEditor.current = next;
    if (remember) memory.editors.set(boundary.repository ?? "", { query: next.query, selected: next.selected, drafts: next.drafts });
    setEditor(next);
  }, [boundary, memory]);
  const load = useCallback(async () => {
    if (!mounted.current || availability.current.detail || !boundary.transport?.listWorkflows || !boundary.repository) return;
    const generation = availability.current.generation;
    const request = ++reads.current;
    setCatalog({ boundary, status: "loading" });
    try {
      const value = await boundary.transport.listWorkflows(boundary.repository);
      if (!mounted.current || currentBoundary.current !== boundary || request !== reads.current || generation !== availability.current.generation) return;
      setCatalog({ boundary, status: "loaded", value });
    } catch (error) {
      if (mounted.current && currentBoundary.current === boundary && request === reads.current && generation === availability.current.generation) setCatalog({ boundary, status: "failed", detail: describe(error) });
    }
  }, [boundary]);

  useEffect(() => {
    mounted.current = true;
    setEditor(emptyEditor(boundary, memory));
    pending.current = null;
    fieldRefs.current.clear();
    focusError.current = null;
    void load();
    return () => { mounted.current = false; reads.current++; };
  }, [boundary, load, memory]);
  useEffect(() => {
    const previous = observedAvailability.current;
    observedAvailability.current = { boundary, unavailable };
    if (unavailable) {
      pending.current = null;
      updateEditor(current => ({ ...current, pending: false, feedback: null }));
    } else if (previous.boundary === boundary && previous.unavailable) void load();
  }, [boundary, unavailable, load, updateEditor]);
  useEffect(() => {
    if (focusError.current === null || savedEditor.boundary !== boundary) return;
    if (focusError.current.type === "form") feedbackRef.current?.focus();
    else fieldRefs.current.get(focusError.current.name)?.focus();
    focusError.current = null;
  }, [savedEditor.errors, savedEditor.feedback, boundary, savedEditor.boundary]);

  const items = catalog.status === "loaded" ? catalog.value.items : [];
  const selected = items.find(entry => entry.id === editor.selected);
  const filtered = items.filter(entry => `${entry.id} ${entry.description ?? ""} ${origin(entry)}`.toLocaleLowerCase().includes(editor.query.trim().toLocaleLowerCase()));
  const canonical = catalog.status === "loaded" && typeof catalog.value.repository === "string" && catalog.value.repository === repository;
  const drafts = selected ? editor.drafts.get(selected.id) ?? new Map<string, string>() : new Map<string, string>();
  const parsed = useMemo(() => selected ? parseWorkflowInputs(selected, drafts) : null, [selected, drafts]);
  const signature = selected && parsed && !parsed.errors.size ? workflowIntent(selected.id, selected.definition_digest, parsed.inputs) : null;
  const requestSignature = signature === null ? null : `catalog\0${repository}\0${signature}`;
  const retrying = requestSignature !== null && unresolved.has(requestSignature);

  const start = async () => {
    if (!mounted.current || availability.current.detail || !selected || !parsed || !canonical || !boundary.transport?.startWorkflow || pending.current?.boundary === boundary) return;
    if (parsed.errors.size) {
      const first = parsed.errors.keys().next().value;
      focusError.current = first === undefined ? { type: "form" } : { type: "field", name: first };
      updateEditor(current => ({ ...current, errors: parsed.errors, feedback: { type: "error", text: "Review the highlighted inputs before starting." } }));
      return;
    }
    const intent = `catalog\0${repository}\0${workflowIntent(selected.id, selected.definition_digest, parsed.inputs)}`;
    const key = unresolved.get(intent) ?? newWorkflowRequestKey();
    unresolved.set(intent, key);
    const preference = preferWorkflowRun(memory, boundary.repository ?? "");
    const operation = { boundary, key };
    const generation = availability.current.generation;
    pending.current = operation;
    updateEditor(current => ({ ...current, pending: true, errors: new Map(), feedback: null }));
    try {
      const runId = await boundary.transport.startWorkflow(selected.id, parsed.inputs, { expectedRepository: boundary.repository ?? undefined, expectedDefinitionDigest: selected.definition_digest, idempotencyKey: key });
      if (!runId.trim()) throw new Error("The runtime did not return a workflow run identity.");
      // A receipt still belongs to its owner after view navigation. Remember it
      // for the next visit without navigating or focusing an abandoned view.
      const preferred = memory.transport === boundary.transport &&
        rememberWorkflowRun(memory, boundary.repository ?? "", preference, runId);
      if (!mounted.current || currentBoundary.current !== boundary || pending.current !== operation || generation !== availability.current.generation) return;
      // A background receipt must not turn another view's visible Retry into
      // a new paid launch. Retain its identity until the current view confirms it.
      unresolved.delete(intent);
      updateEditor(current => ({ ...current, pending: false, feedback: { type: "success", text: `Workflow started: ${runId}` } }));
      pending.current = null;
      if (preferred) latestStarted.current(runId);
    } catch (error) {
      if (!mounted.current || currentBoundary.current !== boundary || pending.current !== operation || generation !== availability.current.generation) return;
      pending.current = null;
      focusError.current = { type: "form" };
      updateEditor(current => ({ ...current, pending: false, feedback: { type: "error", text: `The start request could not be confirmed. Your inputs were kept. Retrying these inputs uses the same request identity. ${describe(error)}` } }));
    }
  };

  const choose = (entry: WorkflowCatalogEntry) => updateEditor(current => ({ ...current, selected: entry.id, errors: new Map(), feedback: null }), true);
  const changeInput = (name: string, value: string) => {
    if (!selected) return;
    updateEditor(current => {
      const entries = new Map(current.drafts), values = new Map(entries.get(selected.id));
      values.set(name, value); entries.set(selected.id, values);
      const errors = new Map(current.errors); errors.delete(name);
      return { ...current, drafts: entries, errors, feedback: null };
    }, true);
  };

  return <section className="workflow-launcher" aria-labelledby={`${prefix}-title`}>
    <header className="workflow-launcher-header"><div><h3 id={`${prefix}-title`}><GitBranch size={19} aria-hidden="true" /> Workflow Library</h3>
      <p>Choose a definition, review its inputs, then start a durable run.</p></div>
      <button type="button" onClick={() => void load()} disabled={!transport?.listWorkflows || !repository || catalog.status === "loading" || editor.pending || Boolean(unavailable)}><RefreshCw size={14} aria-hidden="true" /> Refresh Library</button></header>
    {unavailable && <p className="workflow-launcher-warning" role="status">Workflows are unavailable: {unavailable} Your inputs are kept. No start requests are replayed automatically.</p>}
    {!repository ? <div className="workflow-launcher-state" role="status"><strong>Choose a Repository</strong><p>Workflow definitions and runs belong to the selected checkout.</p>{onOpenRepository && <button type="button" onClick={onOpenRepository}>Choose Repository</button>}</div>
      : !transport?.listWorkflows ? <div className="workflow-launcher-state" role="status"><strong>Workflow Library Unavailable</strong><p>The connected runtime does not provide workflow discovery. Reconnect or update it to browse definitions.</p></div>
      : unavailable && catalog.status !== "loaded" ? <div className="workflow-launcher-state"><strong>Reconnect to Browse Workflows</strong><p>The library could not be confirmed for this connection.</p></div>
      : catalog.status === "idle" || catalog.status === "loading" ? <div className="workflow-launcher-state" role="status">Loading workflow definitions…</div>
      : catalog.status === "failed" ? <div className="workflow-launcher-state" role="alert"><strong>Could Not Load Workflows</strong><p>{catalog.detail}</p><button type="button" onClick={() => void load()}>Try Again</button></div>
      : <>
        {!canonical && <p className="workflow-launcher-warning" role="alert">The runtime did not confirm the selected repository. Refresh the library before starting a workflow.</p>}
        {catalog.value.truncated && <p className="workflow-launcher-warning" role="status"><AlertTriangle size={15} aria-hidden="true" /> This library is partial. Filtering searches only the loaded definitions.</p>}
        {catalog.value.diagnostics.length > 0 && <details className="workflow-launcher-diagnostics"><summary>Some Workflow Definitions Could Not Be Loaded ({catalog.value.diagnostics.length})</summary>
          <ul>{catalog.value.diagnostics.map((diagnostic, index) => <li key={index}>{diagnostic.workflow_id && <strong>{diagnostic.workflow_id}: </strong>}{diagnostic.message}<code>{diagnostic.code}</code></li>)}</ul></details>}
        {items.length === 0 ? <div className="workflow-launcher-state" role="status"><strong>No Workflow Definitions</strong><p>{catalog.value.diagnostics.length ? "Review the definition errors above, then refresh the library." : "The runtime returned no workflow definitions for this repository."}</p></div>
          : <div className="workflow-launcher-grid"><div className="workflow-launcher-list">
            <label className="workflow-launcher-search"><Search size={15} aria-hidden="true" /><span className="workflow-launcher-sr-only">Filter Loaded Workflows</span><input value={editor.query} placeholder="Filter loaded workflows…" onChange={event => updateEditor(current => ({ ...current, query: event.target.value }), true)} disabled={editor.pending} /></label>
            <p className="workflow-launcher-count">{filtered.length} of {items.length} loaded definitions</p>
            <ul aria-label="Available Workflows">{filtered.map(entry => <li key={entry.id}><button type="button" aria-pressed={selected?.id === entry.id} disabled={editor.pending || Boolean(unavailable)} onClick={() => choose(entry)}>
              <strong>{entry.id}</strong>{entry.description && <span>{entry.description}</span>}<small>{origin(entry)} · v{entry.version} · {entry.node_count} steps</small>
            </button></li>)}</ul>
            {filtered.length === 0 && <p className="workflow-launcher-state">No loaded definitions match this filter.</p>}
          </div>
          <div className="workflow-launcher-editor">{selected ? <form onSubmit={event => { event.preventDefault(); void start(); }} aria-label={`Start ${selected.id}`} aria-busy={editor.pending}>
            <h4>{selected.id}</h4>{selected.description && <p>{selected.description}</p>}<p className="workflow-launcher-meta">{origin(selected)} definition · Version {selected.version} · {selected.node_count} steps</p>
            <p className="workflow-launcher-help">Enter each value as JSON. Put text in double quotes. Leave optional values blank to omit them; <code>null</code> is an explicit value. Declared types come from the definition; enter the JSON value it expects. Combined inputs must fit within 65,536 bytes.</p>
            {workflowInputEntries(selected).map(([name, specification], index) => <div className="workflow-launcher-field" key={name}>
              <label htmlFor={`${prefix}-input-${index}`}><strong>{name}</strong><span>{specification.required ? "Required" : "Optional"} · {specification.input_type}</span></label>
              <textarea id={`${prefix}-input-${index}`} ref={element => { if (element) fieldRefs.current.set(name, element); else fieldRefs.current.delete(name); }}
                value={drafts.get(name) ?? ""} onChange={event => changeInput(name, event.target.value)} disabled={editor.pending || Boolean(unavailable)}
                aria-required={specification.required} aria-invalid={editor.errors.has(name)} aria-describedby={editor.errors.has(name) ? `${prefix}-error-${index}` : undefined} maxLength={MAX_WORKFLOW_INPUT_BYTES} spellCheck={false} placeholder="Enter a JSON value" rows={3} />
              {editor.errors.has(name) && <p className="workflow-launcher-input-error" id={`${prefix}-error-${index}`}>{editor.errors.get(name)}</p>}
            </div>)}
            {workflowInputEntries(selected).length === 0 && <p className="workflow-launcher-help">This definition declares no inputs.</p>}
            {!transport.startWorkflow && <p className="workflow-launcher-warning" role="status">This runtime cannot start workflows.</p>}
            {editor.feedback && <p className={`workflow-launcher-feedback ${editor.feedback.type}`} role={editor.feedback.type === "error" ? "alert" : "status"} ref={feedbackRef} tabIndex={-1}>{editor.feedback.text}</p>}
            <div className="workflow-launcher-submit"><span>Listing definitions does not check model or tool readiness.</span><button type="submit" disabled={!canonical || !transport.startWorkflow || editor.pending || Boolean(unavailable)}><Play size={14} aria-hidden="true" />{editor.pending ? "Starting…" : retrying ? "Retry Start" : "Start Workflow"}</button></div>
          </form> : <div className="workflow-launcher-state"><GitBranch size={24} aria-hidden="true" /><strong>Choose a Workflow</strong><p>Selecting a definition only opens its inputs. Work starts when you choose Start Workflow.</p></div>}</div>
        </div>}
      </>}
  </section>;
}
