import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ExternalLink } from "lucide-react";
import type { WorkspaceTarget } from "../types.js";

const EDITOR_PREFERENCE = "codypendent.workspace.editor";
export function preferredWorkspaceEditor(): string | null {
  try { return localStorage.getItem(EDITOR_PREFERENCE); } catch { return null; }
}

/** The shell validates the saved checkout again when an installed app is opened. */
export function WorkspaceLauncher({ repository, sessionId }: { repository?: string | null; sessionId?: string | null }) {
  const [targets, setTargets] = useState<WorkspaceTarget[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [opening, setOpening] = useState(false);
  const latestRepository = useRef(repository);
  latestRepository.current = repository;
  useEffect(() => { setError(null); }, [repository]);
  useEffect(() => {
    let current = true;
    void invoke<WorkspaceTarget[]>("workspace_targets").then((rows) => {
      if (current) setTargets(rows);
    }).catch((error: unknown) => {
      if (current) setError(String(error));
    });
    return () => { current = false; };
  }, []);

  const open = async (target: string) => {
    if (!target || !repository || opening) return;
    setOpening(true);
    setError(null);
    try {
      await invoke<void>("workspace_open", { target, expectedRepository: repository, expectedSession: sessionId ?? null });
      if (targets.find((item) => item.id === target)?.kind === "editor") {
        try { localStorage.setItem(EDITOR_PREFERENCE, target); } catch { /* optional preference */ }
      }
    }
    catch (error) { if (latestRepository.current === repository) setError(String(error)); }
    finally { setOpening(false); }
  };
  return (
    <div className="workspace-launcher">
      <label className="workspace-launcher-control" title={repository ? "Open this repository in an installed application" : "Choose a repository first"}>
        <ExternalLink size={14} aria-hidden="true" />
        <select aria-label="Open repository in" value="" disabled={!repository || opening || targets.length === 0}
          onChange={(event) => void open(event.target.value)}>
          <option value="">{opening ? "Opening…" : "Open repository in…"}</option>
          {(["editor", "terminal", "files"] as const).map((kind) => {
            const rows = targets.filter((target) => target.kind === kind);
            return rows.length ? <optgroup key={kind} label={kind === "editor" ? "Editors" : kind === "terminal" ? "Terminals" : "Files"}>
              {rows.map((target) => <option key={target.id} value={target.id}>{target.label}</option>)}
            </optgroup> : null;
          })}
        </select>
      </label>
      {error && <div className="workspace-launcher-error" role="alert">
        <span>{error}</span><button type="button" aria-label="Dismiss open application error" onClick={() => setError(null)}>×</button>
      </div>}
    </div>
  );
}
