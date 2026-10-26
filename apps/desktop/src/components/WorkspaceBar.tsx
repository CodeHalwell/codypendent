import React from "react";
import { FolderGit2, PanelLeft, Search, Monitor, Sun, Moon, ChevronDown, Plus } from "lucide-react";
import type { Appearance } from "../useWorkspacePreferences.js";
import type { ConnectionStatus } from "../types.js";
import type { DesktopView } from "./Navigation.js";
import { shellAvailable } from "./localConfig.js";
import { WorkspaceLauncher } from "./WorkspaceLauncher.js";
import { viewLabel } from "./ViewBar.js";

interface WorkspaceBarProps {
  sidebarToggleRef?: React.Ref<HTMLButtonElement>;
  view: DesktopView;
  sessionTitle?: string | null;
  sessionId?: string | null;
  repository?: string | null;
  connectionStatus: ConnectionStatus;
  sidebarVisible: boolean;
  onToggleSidebar: () => void;
  appearance: Appearance;
  onAppearanceChange: (appearance: Appearance) => void;
  onOpenPalette: () => void;
  onOpenRepository: () => void;
  onNewSession?: () => void;
  newSessionDisabled?: boolean;
}

export const WorkspaceBar = React.memo(function WorkspaceBar({
  view, sessionTitle, sessionId, repository, connectionStatus, sidebarVisible, sidebarToggleRef,
  onToggleSidebar, appearance, onAppearanceChange, onOpenPalette, onOpenRepository,
  onNewSession, newSessionDisabled,
}: WorkspaceBarProps) {
  const folder = repository?.split(/[\\/]/).filter(Boolean).at(-1);
  const AppearanceIcon = appearance === "system" ? Monitor : appearance === "light" ? Sun : Moon;
  return (
    <header className="workspace-bar" data-connection={connectionStatus}>
      <button ref={sidebarToggleRef} className="workspace-icon-button" type="button" onClick={onToggleSidebar}
        aria-label={sidebarVisible ? "Hide sidebar" : "Show sidebar"} aria-expanded={sidebarVisible}
        title="Toggle sidebar (⌘B / Ctrl-B)"><PanelLeft size={17} aria-hidden="true" /></button>
      <div className="workspace-heading">
        <span className="workspace-title" title={view === "sessions" ? sessionTitle || "New chat" : viewLabel(view)}>
          {view === "sessions" ? sessionTitle || "New chat" : viewLabel(view)}
        </span>
        <span className="workspace-subtitle">Codypendent workspace</span>
      </div>
      <button type="button" className="workspace-repository" onClick={onOpenRepository}
        title={repository ?? "Choose a repository"} aria-label={repository ? `Repository: ${repository}` : "Choose a repository"}>
        <FolderGit2 size={15} aria-hidden="true" />
        <span>{folder ?? "Choose repository"}</span><ChevronDown size={12} aria-hidden="true" />
      </button>
      {onNewSession && <button type="button" className="workspace-new-chat" onClick={onNewSession}
        disabled={newSessionDisabled} aria-label="New chat"
        title={newSessionDisabled ? "Finish or cancel the current run before starting a new chat" : "New chat (⌘N / Ctrl-N) · keeps your draft"}>
        <Plus size={15} aria-hidden="true" /><span>New chat</span>
      </button>}
      {shellAvailable() && <WorkspaceLauncher repository={repository} sessionId={sessionId} />}
      <button type="button" className="workspace-icon-button" onClick={onOpenPalette}
        aria-label="Search commands" title="Search commands (⌘K / Ctrl-K)"><Search size={17} aria-hidden="true" /></button>
      <label className="workspace-appearance" title="Appearance">
        <AppearanceIcon size={16} aria-hidden="true" />
        <select aria-label="Appearance" value={appearance} onChange={(event) => onAppearanceChange(event.target.value as Appearance)}>
          <option value="system">System</option><option value="light">Light</option><option value="dark">Dark</option>
        </select>
      </label>
    </header>
  );
});
