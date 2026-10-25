import React, { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Boxes, Search, Sparkles, Trash2 } from "lucide-react";
import { PageShell, EmptyState } from "../components/PageShell.js";
import { surfaceButton } from "../components/surfaceChrome.js";
import { shellAvailable } from "../components/localConfig.js";
import { HOST_TOOLS } from "./hostTools.js";
import type { DesktopTool } from "./types.js";
import "./agentTools.css";

export function AgentToolsView({ sessionId, connected, error: connectionError, onCompose }: {
  sessionId: string | null; connected: boolean; error: string | null; onCompose: (text: string) => void;
}) {
  const [loaded, setLoaded] = useState<{ sessionId: string; tools: DesktopTool[] } | null>(null);
  const [failure, setFailure] = useState<{ scope: string; detail: string } | null>(null);
  const scope = `${sessionId ?? ""}:${connected}`;
  const currentScope = useRef(scope);
  currentScope.current = scope;
  const generation = useRef(0);
  const mutation = useRef(0);
  const tools = loaded?.sessionId === sessionId ? loaded.tools : null;
  const error = failure?.scope === scope ? failure.detail : null;
  const [query, setQuery] = useState("");
  const [removing, setRemoving] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<string | null>(null);
  const load = useCallback(async () => {
    const request = ++generation.current;
    if (!sessionId || !connected) return;
    setFailure(null);
    if (!shellAvailable()) { setFailure({ scope, detail: "Open the desktop application to load saved tools." }); return; }
    try {
      const tools = await invoke<DesktopTool[]>("desktop_tools_list", { sessionId });
      if (request === generation.current && currentScope.current === scope) setLoaded({ sessionId, tools });
    } catch (reason) {
      if (request === generation.current && currentScope.current === scope) setFailure({ scope, detail: String(reason) });
    }
  }, [sessionId, connected, scope]);
  useEffect(() => {
    setLoaded(null);
    setConfirm(null);
    setRemoving(null);
    setFailure(null);
    void load();
    return () => { generation.current++; mutation.current++; };
  }, [load]);
  const remove = async (tool: DesktopTool) => {
    if (!sessionId || !connected || currentScope.current !== scope) return;
    const request = ++mutation.current;
    const current = () => mutation.current === request && currentScope.current === scope;
    setRemoving(tool.name);
    setFailure(null);
    try {
      await invoke("desktop_tools_remove", { sessionId, name: tool.name, expectedVersion: tool.version });
      if (!current()) return;
      setConfirm(null);
      await load();
    } catch (reason) { if (current()) setFailure({ scope, detail: String(reason) }); }
    finally { if (current()) setRemoving(null); }
  };
  const matches = (tool: DesktopTool) => `${tool.name} ${tool.description}`.toLowerCase().includes(query.toLowerCase());
  return <PageShell className="agent-tools-page">
    <div className="agent-tools-heading"><div><h1>Agent tools</h1><p>Let your agent work with your workspace and build tools of its own.</p></div><Boxes size={28} aria-hidden="true" /></div>
    <div className="agent-tools-intro"><Sparkles size={22} aria-hidden="true" /><div><strong>Ask for what you need.</strong><p>Interactive tables, diagrams, calculators and reusable helpers can be created in the chat. Saved tools belong to that chat.</p></div>
      <button style={surfaceButton()} onClick={() => onCompose("Build an interactive project overview, then create a reusable tool for summarising this workspace.")}>Try it in chat</button></div>
    <label className="agent-tools-search"><Search size={17} aria-hidden="true" /><input aria-label="Search agent tools" placeholder="Find a tool or capability…" value={query} onChange={(event) => setQuery(event.target.value)} /></label>
    {(error || connectionError) && <div role="alert" className="agent-tools-error">{error ?? connectionError} <button style={surfaceButton()} onClick={() => void load()}>Retry</button></div>}
    <div className="agent-tools-section"><h2>Created for this chat <span>{tools?.length ?? ""}</span></h2><button style={surfaceButton()} disabled={!sessionId || !connected} onClick={() => void load()}>Refresh</button></div>
    {!sessionId ? <EmptyState title="Start a chat to create a tool" description="Your agent can save useful helpers here as you work." />
      : !connected ? <EmptyState state="unavailable" title="Workspace reconnecting" description="Your saved tools will be available when the connection returns." />
      : tools === null && !error ? <EmptyState state="loading" title="Loading saved tools…" />
      : tools?.length === 0 ? <EmptyState title="Ready for your first helper" description="Ask your agent to create a reusable tool for a repeated task." />
      : tools && tools.filter(matches).length === 0 ? <EmptyState title="No matching saved tools" description="Try another name or phrase." />
      : <div className="agent-tools-grid">{tools?.filter(matches).map((tool) => <ToolCard key={tool.name} tool={tool}>
        {confirm === tool.name ? <div className="agent-tools-confirm"><span>Remove this saved tool?</span><button disabled={removing !== null} onClick={() => void remove(tool)}>Remove</button><button onClick={() => setConfirm(null)}>Keep tool</button></div>
          : <button className="agent-tool-remove" aria-label={`Remove ${tool.name}`} disabled={removing !== null} onClick={() => setConfirm(tool.name)}><Trash2 size={15} /></button>}
      </ToolCard>)}</div>}
    <h2>Workspace capabilities <span className="agent-tools-count">{HOST_TOOLS.length}</span></h2>
    <p className="agent-tools-muted">These are available alongside the connected agent’s coding tools. Generated helpers run in isolation and can use only their declared capabilities.</p>
    <div className="agent-tools-grid">{HOST_TOOLS.filter(matches).map((tool) => <ToolCard key={tool.name} tool={tool} />)}</div>
  </PageShell>;
}

const toolPresentation: Record<string, { title: string; description: string }> = {
  "desktop.getWorkspace": { title: "Workspace", description: "See which project, model and chat you’re working with." },
  "desktop.navigate": { title: "Open a page", description: "Move between chats, documents, tasks and settings." },
  "desktop.focusComposer": { title: "Return to chat", description: "Bring the message box into focus." },
  "desktop.readDraft": { title: "Read current draft", description: "Use your unsent message as context when you ask for help." },
  "desktop.setDraft": { title: "Prepare a message", description: "Place a suggested message in the composer for you to review." },
  "desktop.setAppearance": { title: "Appearance", description: "Switch between light, dark and your system appearance." },
  "desktop.searchChats": { title: "Find a chat", description: "Search your previous conversations." },
  "desktop.listModels": { title: "Models", description: "See the models you can use." },
  "desktop.listProviders": { title: "Providers", description: "See connected providers and installed agents." },
  "desktop.listSkills": { title: "Skills", description: "Explore the knowledge and tools available to your agents." },
  "desktop.listDocuments": { title: "Documents", description: "Find the notes and plans kept in your workspace." },
  "desktop.listMemory": { title: "Memory", description: "Review useful context your agents have remembered." },
  "desktop.openFile": { title: "Open a file", description: "Jump to a project file in your editor." },
};

function ToolCard({ tool, children }: { tool: DesktopTool; children?: React.ReactNode }) {
  const presentation = toolPresentation[tool.name];
  const title = presentation?.title ?? tool.name.replace(/^desktop\.(custom\.)?/, "").replace(/([a-z])([A-Z])/g, "$1 $2").replace(/[_-]/g, " ");
  return <article className="agent-tool-card"><div className="agent-tool-title"><strong>{title}</strong>{children}</div><p>{presentation?.description ?? tool.description}</p>
    {tool.capabilities.length > 0 && <div className="agent-tool-capabilities">{tool.capabilities.map((name) => <span key={name}>{name.replace("desktop.", "")}</span>)}</div>}
    <details><summary>Tool details{tool.functionBody ? ` · version ${tool.version}` : ""}</summary><code>{tool.name}</code>{presentation && <p>{tool.description}</p>}<pre>{JSON.stringify(tool.parameters, null, 2)}</pre>{tool.functionBody && <pre>{tool.functionBody}</pre>}</details>
  </article>;
}
