import React from "react";
import { ArrowUpRight, BookOpen, ChevronRight, CircleHelp, GitBranch, Inbox, Layers3, MessageSquare, Plus, Search, Settings2, UsersRound, X } from "lucide-react";
import "./Navigation.css";
import { useFocusTrap } from "../useFocusTrap.js";
import { relativeTime } from "../time.js";
import type { ConnectionStatus, SessionSummary, SessionId } from "../types.js";
import type { ConnectionInfo } from "../transport.js";

export type DesktopView =
  | "onboarding"
  | "sessions"
  | "inbox"
  | "analytics"
  | "skills"
  | "memory"
  | "docs"
  | "plugins"
  | "agentTools"
  | "context"
  | "library"
  | "workflow"
  | "board"
  | "blackboard"
  | "repository"
  | "council"
  | "models"
  | "providers"
  | "keys"
  | "mode"
  | "councilResults"
  | "edges"
  | "backtrack";

/**
 * The views the sidebar offers, grouped.
 *
 * This is the ONE table of desktop surfaces. The sidebar renders it and the
 * command palette derives from it (`CommandPalette.completePaletteEntries`),
 * so a view cannot be added to one and forgotten in the other — the defect
 * that shipped four fully-built, unreachable pickers last wave.
 *
 * Each row is `[view, sidebar label, palette description]`. The description
 * is what the palette shows when the app does not supply a richer entry of
 * its own for that view.
 */
export const NAV_GROUPS = [
  {
    // First-run setup. LOCAL CONFIG like the Configuration group below — it
    // reads `models.toml`, `auth.json` and the stored repository preference,
    // never the daemon — but it sits first because it is the answer to "I just
    // installed this and nothing tells me what to do".
    group: "Setup",
    views: [
      [
        "onboarding",
        "Get Started",
        "check what first-run setup still needs: a model, a credential, a repository",
      ],
    ],
  },
  {
    group: "Run",
    views: [
      ["sessions", "Sessions", "the attached session's transcript and composer"],
      ["library", "All chats", "find saved chats and organise your conversations"],
      ["inbox", "Inbox", "notifications and human work from the durable inbox"],
      ["analytics", "Usage", "review model usage, time and measured results"],
      ["context", "Context", "token usage breakdown for the active run"],
      // Reads the attached session's own ledger for its recorded checkpoints and
      // acts on them with `ForkSession` / `RestoreCheckpoint`, so it belongs here.
      ["backtrack", "Checkpoints", "continue from an earlier point in this chat"],
    ],
  },
  {
    group: "Workflow",
    views: [
      ["workflow", "Workflows", "start a workflow and follow its progress"],
      ["board", "Task Board", "create, assign, and move repository backlog tasks in Kanban columns"],
      ["blackboard", "Shared results", "inspect the workflow blackboard, shared evidence and open questions"],
    ],
  },
  {
    group: "Workspace",
    views: [
      ["repository", "Repository", "choose the git checkout every repository-scoped command anchors to"],
      ["docs", "Documents", "create, edit and publish workspace documents"],
      ["skills", "Skills", "inspect registered skills and their permissions"],
      ["memory", "Memory", "browse curated memories and their provenance"],
      ["plugins", "Extensions", "manage verified workspace extensions"],
      ["agentTools", "Agent tools", "inspect reusable agent tools and available workspace capabilities"],
      // Daemon-backed like the rest of this group: `ReadCodeGraph` /
      // `ReadCodeGraphStatus` over the socket, never a local index.
      ["edges", "Code Graph", "search the stored code graph one bounded page of nodes and edges at a time"],
    ],
  },
  {
    // Model, provider, key and mode selection are LOCAL CONFIG (models.toml,
    // auth.json) rather than daemon protocol, so they group apart from the
    // daemon-backed views above — same reasoning as Councils below.
    group: "Configuration",
    views: [
      ["models", "Models", "choose the model pinned to your next and later runs (models.toml)"],
      ["providers", "Providers", "inspect and edit the configured providers (models.toml)"],
      ["keys", "API Keys", "inspect and set provider credentials (auth.json)"],
      ["mode", "Mode", "choose the submission mode for the next run"],
    ],
  },
  {
    // Councils are local configuration, not protocol — grouped apart from the
    // daemon-backed views so the distinction is visible in the chrome itself.
    group: "Councils",
    views: [
      ["council", "Councils", "list, run, and manage persisted multi-model councils"],
      ["councilResults", "Council Results", "open durable council outcomes"],
    ],
  },
] as const satisfies ReadonlyArray<{
  readonly group: string;
  readonly views: ReadonlyArray<readonly [DesktopView, string, string]>;
}>;

/** Every view `NAV_GROUPS` actually lists. */
type NavigatedView = (typeof NAV_GROUPS)[number]["views"][number][0];

/**
 * Compile-time proof that `NAV_GROUPS` covers `DesktopView` exhaustively.
 *
 * Adding a member to the union without adding it to the table above fails
 * `tsc --noEmit` right here, which is the whole point: the sidebar and the
 * palette both read that table, so an uncovered view is unreachable by BOTH.
 */
export const NAV_COVERS_EVERY_VIEW: [DesktopView] extends [NavigatedView] ? true : never = true;

interface NavigationProps {
  hidden?: boolean;
  modal?: boolean;
  onClose?: () => void;
  restoreFocusFallback?: React.RefObject<HTMLElement | null>;
  sessions: SessionSummary[];
  activeSessionId: SessionId | null;
  onSelectSession: (id: SessionId) => void;
  connectionStatus: ConnectionStatus;
  statusDetail?: string;
  connectionInfo?: ConnectionInfo | null;
  currentView?: DesktopView;
  onSelectView?: (view: DesktopView) => void;
  unreadInboxCount?: number;
  onOpenPalette?: () => void;
  onNewSession?: () => void;
  newSessionDisabled?: boolean;
  unavailableViews?: ReadonlySet<DesktopView>;
}

const PRIMARY_VIEWS: ReadonlySet<DesktopView> = new Set(["sessions", "library", "inbox"]);
const WORKSPACE_TOOLS_PREFERENCE = "codypendent.workspaceTools";

function workspaceToolsExpanded(): boolean {
  try { return localStorage.getItem(WORKSPACE_TOOLS_PREFERENCE) !== "collapsed"; }
  catch { return true; }
}
const TOOL_GROUP_ORDER = ["Councils", "Workflow", "Workspace", "Run", "Configuration", "Setup"];
const TOOL_GROUPS = [...NAV_GROUPS].sort((a, b) => TOOL_GROUP_ORDER.indexOf(a.group) - TOOL_GROUP_ORDER.indexOf(b.group));
const GROUP_ICONS = {
  Councils: UsersRound, Workflow: GitBranch, Workspace: Layers3,
  Run: ArrowUpRight, Configuration: Settings2, Setup: CircleHelp,
};
const PRIMARY_LINKS = [
  { view: "sessions", label: "Chat", accessibleLabel: "Sessions", icon: MessageSquare },
  { view: "library", label: "All chats", accessibleLabel: "Session Library", icon: BookOpen },
  { view: "inbox", label: "Inbox", accessibleLabel: "Inbox", icon: Inbox },
] as const;

export const Navigation: React.FC<NavigationProps> = React.memo(function Navigation({
  hidden = false, modal = false, onClose, restoreFocusFallback, sessions, activeSessionId, onSelectSession, connectionStatus,
  statusDetail, connectionInfo, currentView = "sessions", onSelectView,
  unreadInboxCount = 0, onOpenPalette, onNewSession, newSessionDisabled, unavailableViews,
}: NavigationProps) {
  const connected = connectionStatus === "connected";
  const asideRef = React.useRef<HTMLElement>(null);
  const [openGroups, setOpenGroups] = React.useState<ReadonlySet<string>>(() => new Set());
  const [toolsExpanded, setToolsExpanded] = React.useState(workspaceToolsExpanded);
  const toolsId = React.useId();
  const [filter, setFilter] = React.useState("");
  const [filterOpen, setFilterOpen] = React.useState(false);
  const filterRef = React.useRef<HTMLInputElement>(null);
  const filterToggleRef = React.useRef<HTMLButtonElement>(null);
  const activeGroup = PRIMARY_VIEWS.has(currentView) ? undefined
    : NAV_GROUPS.find(({ views }) => views.some(([view]) => view === currentView))?.group;
  const visibleSessions = React.useMemo(() => {
    const query = filter.trim().toLocaleLowerCase();
    return query ? sessions.filter((session) => (session.title || "Untitled Session").toLocaleLowerCase().includes(query)) : sessions;
  }, [sessions, filter]);

  const toggleGroup = (group: string) => setOpenGroups((open) => {
    const next = new Set(open);
    if (!next.delete(group)) next.add(group);
    return next;
  });
  const toggleFilter = () => {
    setFilterOpen((open) => !open);
    setFilter("");
  };
  const choose = (action: (() => void) | undefined) => {
    if (modal) onClose?.();
    action?.();
  };
  useFocusTrap(asideRef, { active: modal && !hidden, restoreFocusFallback, onEscape: (event) => {
    if (event.target === filterRef.current) {
      toggleFilter();
      filterToggleRef.current?.focus();
    } else onClose?.();
  } });
  React.useEffect(() => {
    if (activeGroup) setOpenGroups((open) => new Set([...open, activeGroup]));
  }, [activeGroup]);
  React.useEffect(() => { if (filterOpen) filterRef.current?.focus(); }, [filterOpen]);
  React.useEffect(() => {
    try { localStorage.setItem(WORKSPACE_TOOLS_PREFERENCE, toolsExpanded ? "expanded" : "collapsed"); }
    catch { /* A restricted webview can still use the disclosure without persistence. */ }
  }, [toolsExpanded]);

  return (
    <aside ref={asideRef} hidden={hidden} className="workspace-sidebar cody-navigation" aria-label="Workspace sidebar"
      role={modal ? "dialog" : undefined} aria-modal={modal ? true : undefined}
      style={{ display: hidden ? "none" : "flex" }}>
      <div className="cody-nav-brand">
        <img src="/codypendent-mark.png" alt="" className="workspace-brand-mark" width="28" height="28" />
        <span>Codypendent</span>
        <span className="cody-nav-status" data-status={connectionStatus} role="img"
          aria-label={`codypendentd ${connectionStatus}`} title={statusDetail ?? `codypendentd ${connectionStatus}`} />
        {modal && <button type="button" className="cody-nav-close" onClick={onClose} aria-label="Close sidebar" title="Close sidebar">
          <X size={17} aria-hidden="true" />
        </button>}
      </div>

      <div className="cody-nav-actions">
        {onNewSession && <button type="button" className="cody-nav-new" onClick={() => choose(onNewSession)}
          disabled={newSessionDisabled} aria-label="Start new chat"
          title={newSessionDisabled ? "Finish or cancel the current run before starting a new chat" : "New chat (⌘N / Ctrl-N) · keeps your draft"}>
          <Plus size={17} aria-hidden="true" /><span>New chat</span><kbd>⌘N</kbd>
        </button>}
        {onOpenPalette && <button type="button" className="cody-nav-search" onClick={() => choose(onOpenPalette)}
          aria-label="Open command palette" title="Search commands (⌘K / Ctrl-K)">
          <Search size={16} aria-hidden="true" /><span>Search commands</span><kbd>⌘K</kbd>
        </button>}
      </div>

      <nav className="cody-nav-primary" aria-label="Main navigation">
        {PRIMARY_LINKS.map(({ view, label, accessibleLabel, icon: Icon }) => <button key={view} type="button"
          className="workspace-nav-item cody-nav-link" onClick={() => choose(() => onSelectView?.(view))}
          aria-label={`${accessibleLabel} View`} aria-current={currentView === view ? "page" : undefined}
          style={{ fontWeight: currentView === view ? 600 : 400 }}>
          <Icon size={16} aria-hidden="true" /><span>{label}</span>
          {view === "inbox" && unreadInboxCount > 0 && <span className="cody-nav-badge" data-testid="inbox-badge">{unreadInboxCount}</span>}
        </button>)}
      </nav>

      <section className="workspace-recent cody-nav-recents" aria-label="Recent chats">
        <div className="cody-nav-section-heading">
          <span>Recent chats</span>
          <button ref={filterToggleRef} type="button" className="cody-nav-small-button" onClick={toggleFilter}
            aria-label={filterOpen ? "Close chat filter" : "Filter recent chats"} aria-expanded={filterOpen}
            title={filterOpen ? "Close chat filter" : "Filter recent chats"}>
            {filterOpen ? <X size={13} aria-hidden="true" /> : <Search size={13} aria-hidden="true" />}
          </button>
        </div>
        {filterOpen && <input ref={filterRef} className="cody-nav-filter" type="search" value={filter}
          onChange={(event) => setFilter(event.target.value)} placeholder="Find a recent chat…" aria-label="Filter recent chats"
          onKeyDown={(event) => { if (event.key === "Escape") { event.stopPropagation(); toggleFilter(); filterToggleRef.current?.focus(); } }} />}
        <div className="cody-nav-session-list">
          {visibleSessions.length === 0 ? <p className="cody-nav-empty">
            {filter.trim() ? "No matching chats" : connected ? "Your chats will appear here." : "Connect to load your chats."}
          </p> : visibleSessions.map((session) => {
            const active = session.id === activeSessionId && currentView === "sessions";
            return <button key={session.id} type="button" className="workspace-session-item cody-nav-session"
              onClick={() => choose(() => { onSelectView?.("sessions"); onSelectSession(session.id); })}
              aria-label={`Session ${session.title || "Untitled Session"}`} aria-current={active ? "true" : undefined}
              title={session.title || "Untitled Session"}>
              <span className="cody-nav-session-title">{session.title || "Untitled Session"}</span>
              <span className="cody-nav-session-meta"><span>{session.state}</span><span>{relativeTime(session.updated_at)}</span></span>
            </button>;
          })}
        </div>
      </section>

      <nav className="cody-nav-tools" aria-label="Workspace tools" data-collapsed={!toolsExpanded}>
        <button type="button" className="cody-nav-section-heading cody-nav-workspace-toggle"
          aria-expanded={toolsExpanded} aria-controls={toolsId}
          onClick={() => setToolsExpanded((expanded) => !expanded)}>
          <span>Your workspace</span>
          <ChevronRight size={13} aria-hidden="true" className="cody-nav-chevron" data-open={toolsExpanded} />
        </button>
        <div id={toolsId} hidden={!toolsExpanded}>
        {toolsExpanded && TOOL_GROUPS.map(({ group, views: destinations }) => {
          const views = destinations.filter(([view]) => !PRIMARY_VIEWS.has(view));
          const open = openGroups.has(group);
          const Icon = GROUP_ICONS[group as keyof typeof GROUP_ICONS] ?? Layers3;
          return <div className="cody-nav-group" key={group}>
            <button type="button" className="cody-nav-group-toggle" onClick={() => toggleGroup(group)}
              aria-expanded={open} aria-label={`${group} group`}>
              <Icon size={15} aria-hidden="true" /><span>{group}</span>
              <ChevronRight size={12} aria-hidden="true" className="cody-nav-chevron" data-open={open} />
            </button>
            {open && <div className="cody-nav-group-links">{views.map(([view, label]) => <button key={view} type="button"
              className="workspace-nav-item cody-nav-link cody-nav-tool" onClick={() => choose(() => onSelectView?.(view))}
              aria-label={`${label} View`} aria-current={currentView === view ? "page" : undefined}
              style={{ fontWeight: currentView === view ? 600 : 400 }}>
              <span>{label}</span>
              {unavailableViews?.has(view) && <span className="cody-nav-unavailable" data-testid={`nav-unavailable-${view}`}
                title="This build's shell does not provide the data for this view yet">not in this build</span>}
            </button>)}</div>}
          </div>;
        })}
        </div>
      </nav>

      {connected && connectionInfo && <VersionFooter info={connectionInfo} />}
    </aside>
  );
});

const VersionFooter: React.FC<{ info: ConnectionInfo }> = ({ info }) => {
  const mismatch = info.client_version !== undefined && info.client_version !== info.daemon_version;
  return <div className="cody-nav-version" data-testid="version-footer" data-mismatch={mismatch}
    title={`codypendentd ${info.daemon_version} on ${info.socket_path} (build ${info.build_id})`}>
    daemon {info.daemon_version} · protocol {info.protocol_version}
    {info.client_version !== undefined && ` · desktop ${info.client_version}`}
    {mismatch && <div>The daemon is a different version from this app. Restart it after upgrading: <code>codypendent daemon restart</code>.</div>}
  </div>;
};
