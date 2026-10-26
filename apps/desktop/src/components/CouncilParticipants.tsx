import { Crown } from "lucide-react";
import { modelLabel } from "../modelPresentation.js";
import type { CouncilMemberRow } from "../localConfig.js";

export function councilTitle(name: string): string {
  return name.replace(/([a-z])([A-Z])/g, "$1 $2").replace(/[_.-]+/g, " ")
    .replace(/\b[a-z]/g, (letter) => letter.toUpperCase());
}

/** Natural names become portable identifiers; an explicitly typed valid ID stays exact. */
export function councilKey(name: string): string {
  const value = name.trim();
  return /^[A-Za-z0-9._-]+$/.test(value) ? value : value.normalize("NFKD")
    .replace(/[\u0300-\u036f]/g, "").toLowerCase().replace(/[^a-z0-9._-]+/g, "-").replace(/^-+|-+$/g, "");
}

export function councilRole(role: string): string {
  return councilTitle(role || "member");
}

export function repositoryName(path: string): string {
  return path.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || path;
}

export function CouncilModel({ model, role, chair = false }: { model: string; role?: string; chair?: boolean }) {
  const label = modelLabel(model);
  return <div className={`council-model ${chair ? "is-chair" : ""}`}>
    <span className="council-model-avatar" aria-hidden="true">{chair ? <Crown size={15} /> : label.charAt(0)}</span>
    <span className="council-model-copy"><strong>{label}</strong><span>{chair ? "Chair · brings the answers together" : councilRole(role ?? "member")}</span></span>
  </div>;
}

export function CouncilParticipants({ members, chair }: { members: CouncilMemberRow[]; chair?: string }) {
  return <div className="council-team">
    <div className="council-members" aria-label="Council members">
      {members.map((member, index) => <CouncilModel key={`${member.model}:${member.role}:${index}`} model={member.model} role={member.role} />)}
    </div>
    {chair && <div className="council-chair" aria-label="Council chair"><CouncilModel model={chair} chair /></div>}
  </div>;
}
