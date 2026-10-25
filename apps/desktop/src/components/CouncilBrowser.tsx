import { EmptyState, PageShell } from "./PageShell.js";
import React, { useCallback, useRef, useState } from "react";
import { ArrowUpRight, Plus, RefreshCw, Search, UsersRound } from "lucide-react";
import { modelLabel } from "../modelPresentation.js";
import { useLoadOnMount } from "../useLoadOnMount.js";
import { useFocusTrap } from "../useFocusTrap.js";
import { CouncilParticipants, councilTitle } from "./CouncilParticipants.js";
import type { CouncilCard } from "../localConfig.js";
import "./Councils.css";

export interface CouncilBrowserProps {
  onLoad: () => Promise<CouncilCard[]>;
  onDelete?: (name: string) => Promise<void>;
  onRun?: (name: string) => void;
  onCreate?: () => void;
  onOpenResults?: () => void;
}
type Status = { kind: "loading" } | { kind: "loaded"; councils: CouncilCard[] } | { kind: "unavailable"; detail: string };

export const CouncilBrowser: React.FC<CouncilBrowserProps> = ({ onLoad, onDelete, onRun, onCreate, onOpenResults }) => {
  const [status, setStatus] = useState<Status>({ kind: "loading" });
  const [query, setQuery] = useState("");
  const [confirming, setConfirming] = useState<CouncilCard | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);
  const generation = useRef(0);
  const confirmRef = useRef<HTMLDivElement | null>(null);
  const confirmCancelRef = useRef<HTMLButtonElement | null>(null);
  useFocusTrap(confirmRef, { active: confirming !== null, initialFocus: confirmCancelRef,
    onEscape: () => { if (!busyRef.current) setConfirming(null); } });
  const load = useCallback(async () => {
    const current = ++generation.current;
    setStatus({ kind: "loading" });
    try { const councils = await onLoad(); if (generation.current === current) setStatus({ kind: "loaded", councils }); }
    catch (failure) { if (generation.current === current) setStatus({ kind: "unavailable", detail: describe(failure) }); }
  }, [onLoad]);
  useLoadOnMount(load);
  const confirmDelete = async () => {
    if (!onDelete || !confirming || busyRef.current) return;
    busyRef.current = true; setBusy(true); setError(null);
    try { await onDelete(confirming.name); setConfirming(null); await load(); }
    catch (failure) { setError(describe(failure)); }
    finally { busyRef.current = false; setBusy(false); }
  };
  const matching = status.kind === "loaded" ? status.councils.filter((council) =>
    `${council.name} ${councilTitle(council.name)} ${council.description} ${council.members.map((member) => modelLabel(member.model)).join(" ")} ${modelLabel(council.chair)}`.toLowerCase().includes(query.trim().toLowerCase())) : [];
  return <PageShell className="councils-page"><div className="councils-content">
    <header className="councils-header">
      <div><div className="councils-eyebrow"><UsersRound size={14} aria-hidden="true" /> More than one perspective</div>
        <h1>Councils</h1><p>Bring different models to the same question. Your chair turns their perspectives into one considered answer.</p></div>
      <div className="councils-header-actions">
        {onOpenResults && <button type="button" className="council-button quiet" onClick={onOpenResults}>Saved results</button>}
        {onCreate && <button type="button" className="council-button primary" onClick={onCreate} data-testid="council-new"><Plus size={15} aria-hidden="true" /> New council</button>}
      </div>
    </header>
    <div className="councils-toolbar"><h2>Your councils</h2>
      {status.kind === "loaded" && status.councils.length > 0 && <label className="councils-search"><Search size={14} aria-hidden="true" /><input type="search" aria-label="Find a council" placeholder="Find a council" value={query} onChange={(event) => setQuery(event.target.value)} /></label>}
      <button type="button" onClick={() => void load()} disabled={status.kind === "loading"} className="council-button quiet icon-only" aria-label="Refresh councils" title="Refresh councils" data-testid="council-refresh"><RefreshCw size={14} aria-hidden="true" /></button>
    </div>
    {status.kind === "loading" && <div data-testid="council-loading"><EmptyState state="loading" title="Loading your councils" /></div>}
    {status.kind === "unavailable" && <div data-testid="council-unavailable"><EmptyState state="unavailable" title="Could not load councils" description={status.detail} action={<button type="button" className="council-button" onClick={() => void load()}>Try again</button>} /></div>}
    {status.kind === "loaded" && status.councils.length === 0 && <div data-testid="council-empty"><EmptyState title="A few good perspectives. One clear answer." description="Choose two or more models to contribute, then a chair to bring their findings together. Save the team and come back to it whenever you need."
      action={onCreate && <button type="button" className="council-button primary" onClick={onCreate} data-testid="council-empty-new"><Plus size={15} aria-hidden="true" /> Create your first council</button>} /></div>}
    {status.kind === "loaded" && status.councils.length > 0 && matching.length === 0 && <EmptyState title="No matching councils" description="Try another name, purpose or model." />}
    <div className="councils-list">{matching.map((council) => <article key={council.name} className="council-card" data-testid={`council-${council.name}`}>
      <div className="council-card-header"><div className="council-card-heading"><h2>{councilTitle(council.name)}</h2>
        <p>{council.description || "Different perspectives, brought together by a chair."}</p></div>
        {onRun && <button type="button" className="council-button primary" onClick={() => onRun(council.name)} data-testid={`council-run-${council.name}`}>Start council <ArrowUpRight size={14} aria-hidden="true" /></button>}
      </div>
      <CouncilParticipants members={council.members} chair={council.chair} />
      <div className="council-card-footer"><span>{council.members.length} perspectives · {council.rounds === 1 ? "One discussion round" : `${council.rounds} discussion rounds`}</span>
        {council.evidence && <span>Evidence review enabled</span>}</div>
      <details className="council-details"><summary>Council settings</summary><dl>
        <dt>Council ID</dt><dd>{council.name}</dd><dt>Responses needed</dt><dd>{council.requiredQuorum}{council.quorum == null ? " · majority of members" : " · configured quorum"}</dd>
        <dt>Chair model ID</dt><dd>{council.chair}</dd><dt>Member model IDs</dt><dd>{council.members.map((member) => <div key={member.model}>{member.model}</div>)}</dd>
      </dl>
        {council.chairIsMember && <p className="council-note warning" role="status" data-testid={`council-chair-warning-${council.name}`}>The chair also contributes a member response and will consider its own findings.</p>}
        {onDelete && <button type="button" className="council-button danger" onClick={() => { setError(null); setConfirming(council); }} data-testid={`council-delete-${council.name}`}>Delete council</button>}
      </details>
    </article>)}</div>
    {confirming && <div ref={confirmRef} role="alertdialog" aria-label="Confirm council deletion" className="council-card council-confirm" data-testid="council-delete-confirm">
      <h2>Delete {councilTitle(confirming.name)}?</h2><p>This removes the saved team. Previous council results are kept.</p>
      {error && <div role="alert" className="council-notice error">{error}</div>}
      <div className="council-actions"><button type="button" disabled={busy} onClick={() => void confirmDelete()} className="council-button danger" data-testid="council-delete-yes">{busy ? "Deleting…" : "Delete"}</button>
        <button type="button" ref={confirmCancelRef} disabled={busy} onClick={() => setConfirming(null)} className="council-button" data-testid="council-delete-no">Cancel</button></div>
    </div>}
  </div></PageShell>;
};
function describe(error: unknown) { return error instanceof Error ? error.message : String(error); }
