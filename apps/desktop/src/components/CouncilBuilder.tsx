import { PageShell } from "./PageShell.js";
import React, { useRef, useState } from "react";
import { ArrowLeft, Crown, Plus, UsersRound, X } from "lucide-react";
import { modelLabel } from "../modelPresentation.js";
import { newCouncilDraft } from "../useCouncilRun.js";
import { CouncilParticipants, councilKey, councilTitle } from "./CouncilParticipants.js";
import type { CouncilCard, CouncilDraft, CouncilMemberRow } from "../localConfig.js";
import "./Councils.css";

export interface CouncilBuilderProps {
  onCreate: (draft: CouncilDraft) => Promise<CouncilCard>;
  onCancel?: () => void;
  onCreated?: (council: CouncilCard) => void;
  configuredModels?: string[];
  onOpenModels?: () => void;
  draft?: CouncilDraft;
  onDraftChange?: (draft: CouncilDraft) => void;
}
export const CouncilBuilder: React.FC<CouncilBuilderProps> = ({ onCreate, onCancel, onCreated, configuredModels, onOpenModels, draft, onDraftChange }) => {
  const [localDraft, setLocalDraft] = useState(newCouncilDraft);
  const { name, description, chair, rounds, members } = draft ?? localDraft;
  const updateDraft = (patch: Partial<CouncilDraft>) => {
    const next = { ...(draft ?? localDraft), ...patch };
    if (onDraftChange) onDraftChange(next);
    else setLocalDraft(next);
  };
  const [refusal, setRefusal] = useState<string | null>(null);
  const [created, setCreated] = useState<CouncilCard | null>(null);
  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);
  const updateMember = (index: number, patch: Partial<CouncilMemberRow>) => updateDraft({ members: members.map((member, position) => position === index ? { ...member, ...patch } : member) });
  const filled = members.filter((member) => member.model.trim());
  const key = councilKey(name);
  const duplicate = new Set(filled.map((member) => member.model.trim())).size !== filled.length;
  const blockedBecause = !key ? "Give your council a name." : filled.length < 2 ? "Choose at least two models to contribute."
    : duplicate ? "Choose a different model for each perspective." : !chair.trim() ? "Choose a chair to bring the answers together." : null;
  const submittable = !blockedBecause && !busy && !created;
  const submit = async () => {
    if (!submittable || busyRef.current) return;
    busyRef.current = true; setBusy(true); setRefusal(null);
    try {
      const result = await onCreate({ name: key, description: description.trim(), chair: chair.trim(), rounds,
        members: filled.map((member) => ({ model: member.model.trim(), role: member.role.trim() || "member" })) });
      setCreated(result); onCreated?.(result);
    } catch (error) { setRefusal(error instanceof Error ? error.message : String(error)); }
    finally { busyRef.current = false; setBusy(false); }
  };
  return <PageShell className="councils-page"><div className="councils-content">
    {onCancel && <button type="button" className="council-button quiet" onClick={onCancel} disabled={busy} data-testid="council-builder-cancel"><ArrowLeft size={14} aria-hidden="true" /> Back to councils</button>}
    <header className="councils-header" style={{ marginTop: 16 }}><div><div className="councils-eyebrow"><UsersRound size={14} aria-hidden="true" /> Build your team</div>
      <h1>New council</h1><p>Choose a few models with different strengths. A chair will turn their findings into one answer.</p></div></header>
    <div className="council-form-layout">
      <form className="council-form" onSubmit={(event) => { event.preventDefault(); void submit(); }} aria-label="Create a council">
        <section className="council-form-section"><h2>Give it a purpose</h2>
          <label className="council-label"><span>Council name</span><input className="council-input" data-testid="council-name" value={name} maxLength={64} placeholder="For example, Code review" disabled={busy} onChange={(event) => { updateDraft({ name: event.target.value }); setCreated(null); }} /></label>
          <label className="council-label"><span>What is this team good at? <small>Optional</small></span><textarea className="council-input" data-testid="council-description" rows={2} value={description} placeholder="A second opinion on architecture, security and code quality." disabled={busy} onChange={(event) => updateDraft({ description: event.target.value })} /></label>
        </section>
        <section className="council-form-section"><h2>Choose the perspectives</h2><p>Each model considers the question and contributes its own findings.</p>
          {configuredModels?.length === 0 && <div className="council-notice"><p>Add or discover models before choosing your team.</p>{onOpenModels && <button type="button" className="council-button" onClick={onOpenModels}>Find models</button>}</div>}
          {members.map((member, index) => <div className="council-member-row" key={index}>
            <span className="council-member-number" aria-hidden="true">{index + 1}</span>
            <ModelChoice data-testid={`council-member-model-${index}`} aria-label={`Member ${index + 1} model`} value={member.model} onChange={(model) => updateMember(index, { model })}
              options={configuredModels} excluded={members.filter((_, position) => position !== index).map((value) => value.model)} disabled={busy} />
            <button type="button" className="council-button quiet icon-only" disabled={busy || members.length <= 2} onClick={() => updateDraft({ members: members.filter((_, position) => position !== index) })} data-testid={`council-member-remove-${index}`} aria-label={`Remove member ${index + 1}`}><X size={15} aria-hidden="true" /></button>
          </div>)}
          <button type="button" className="council-button quiet" disabled={busy} onClick={() => updateDraft({ members: [...members, { model: "", role: "member" }] })} data-testid="council-member-add"><Plus size={14} aria-hidden="true" /> Add another perspective</button>
          <label className="council-label" style={{ marginTop: 24 }}><span><Crown size={13} aria-hidden="true" /> Chair</span>
            <ModelChoice data-testid="council-chair" value={chair} onChange={(chair) => updateDraft({ chair })} options={configuredModels} disabled={busy} />
          </label><p className="council-note">The chair reads the findings, weighs differences and brings the answer together.</p>
          {chair && filled.some((member) => member.model === chair) && <p className="council-note">This model will also contribute its own perspective.</p>}
        </section>
        <details className="council-advanced"><summary>Roles and discussion settings</summary>
          <p className="council-note">Give members a focus, or add another round so they can respond to one another.</p>
          {members.map((member, index) => <label key={index} className="council-role-row"><span>{member.model ? modelLabel(member.model) : `Perspective ${index + 1}`} · role</span><input className="council-input" data-testid={`council-member-role-${index}`} aria-label={`Member ${index + 1} role`} value={member.role} disabled={busy} onChange={(event) => updateMember(index, { role: event.target.value })} placeholder="For example, security reviewer" /></label>)}
          <label className="council-label"><span>Discussion rounds</span><input className="council-input" style={{ width: 100 }} data-testid="council-rounds" type="number" min={1} value={rounds} disabled={busy} onChange={(event) => updateDraft({ rounds: Math.max(1, Number(event.target.value) || 1) })} /></label>
          <p className="council-note">Each extra round runs every member again and can use more time and tokens.</p>
          <dl className="council-details"><dt>Council ID</dt><dd>{key || "Generated from the name"}</dd></dl>
        </details>
        {refusal && <div className="council-notice error" role="alert" data-testid="council-builder-refusal">{refusal}</div>}
        {created && <div className="council-notice success" role="status" data-testid="council-builder-created">{councilTitle(created.name)} is ready. {created.members.length} members, chaired by {modelLabel(created.chair)}.</div>}
        <div className="council-form-footer"><button type="submit" className="council-button primary" disabled={!submittable} data-testid="council-builder-submit">{busy ? "Creating council…" : "Create council"}</button>
          {blockedBecause && !busy && <p className="council-note" data-testid="council-builder-blocked">{blockedBecause}</p>}</div>
      </form>
      <aside className="council-form-preview" aria-label="Your council preview"><h2>{name.trim() ? councilTitle(name.trim()) : "Your council"}</h2>
        {filled.length || chair ? <CouncilParticipants members={filled} chair={chair || undefined} /> : <p className="council-note">Your team will take shape here as you choose its models.</p>}
      </aside>
    </div>
  </div></PageShell>;
};
function ModelChoice({ value, options, excluded = [], onChange, ...props }: {
  value: string; options?: string[]; excluded?: string[]; onChange: (value: string) => void;
  disabled?: boolean; "data-testid"?: string; "aria-label"?: string;
}) {
  if (!options) return <input {...props} className="council-input" value={value} placeholder="Choose a model" onChange={(event) => onChange(event.target.value)} />;
  return <select {...props} className="council-input" value={value} onChange={(event) => onChange(event.target.value)}>
    <option value="">Choose a model</option>{value && !options.includes(value) && <option value={value}>{modelLabel(value)}</option>}
    {options.map((model) => <option key={model} value={model} disabled={model !== value && excluded.includes(model)}>{modelLabel(model)}</option>)}
  </select>;
}
