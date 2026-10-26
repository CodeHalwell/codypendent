import { NAV_GROUPS, type DesktopView } from "../components/Navigation.js";

const recipes = {
  onboarding: { tools: ["desktop.listModels", "desktop.listProviders"], userControls: "Choose a repository and complete only genuinely missing authentication or installation." },
  sessions: { tools: ["desktop.getWorkspace", "desktop.readDraft", "desktop.setDraft", "desktop.focusComposer"], userControls: "Send messages, start or select chats, queue or steer follow-ups, pause/resume/cancel runs and decide approvals." },
  library: { tools: ["desktop.searchChats"], userControls: "Open, rename, pin, archive and export saved chats." },
  inbox: { tools: [], userControls: "Read notifications, resolve approvals/questions and manage inbox items." },
  analytics: { tools: [], userControls: "Inspect usage, measured cost, run history and outcomes." },
  context: { tools: ["desktop.getWorkspace"], userControls: "Inspect the active run's token and retrieved-context breakdown." },
  backtrack: { tools: [], userControls: "Inspect checkpoints, fork a conversation or restore a chosen checkpoint." },
  docs: { tools: ["desktop.listDocuments", "docs.read", "docs.create", "docs.edit", "docs.suggest"], userControls: "Inspect collaboration mode, review proposals and publish documents." },
  skills: { tools: ["desktop.listSkills", "skills.search"], userControls: "Inspect registered skills, their provenance and declared permissions." },
  memory: { tools: ["desktop.listMemory", "memory.remember"], userControls: "Inspect, correct or forget curated memories and their provenance; review, promote, edit, retire or delete learnings. Agent memory writes are proposals." },
  plugins: { tools: [], userControls: "Inspect verified extension lifecycle, smoke-test, enable, review pending permission updates or revoke access. Remote plugin surfaces are not rendered in this build." },
  agentTools: { tools: ["desktop.manageTools", "desktop.defineTool", "desktop.executeTool"], userControls: "Inspect helper source, schema, language, version and capabilities; approve their exact execution." },
  workflow: { tools: ["workflow.query", "workflow.create", "workflow.run"], userControls: "Inspect workflows and durable node progress. Saving a workflow does not start it." },
  board: { tools: ["task.list", "task.create", "task.update", "task.move"], userControls: "Organise repository backlog cards and review status, assignees and history." },
  blackboard: { tools: ["blackboard.query", "blackboard.post"], userControls: "Inspect shared workflow evidence and questions. These tools require a workflow-node context." },
  repository: { tools: ["desktop.getWorkspace", "desktop.openFile", "workspace.search", "workspace.read_file"], userControls: "Choose the repository; repository tools stay scoped to the authorised checkout." },
  council: { tools: ["council.create", "council.run", "council.result", "desktop.listModels"], userControls: "Create, inspect, start or delete saved councils and choose members/chair from available models." },
  councilResults: { tools: ["council.result"], userControls: "Inspect member reports, evidence and persisted chair synthesis." },
  models: { tools: ["desktop.listModels"], userControls: "Discover/test models, choose the next run's model and manage model profiles." },
  providers: { tools: ["desktop.listProviders"], userControls: "Inspect discovery and authentication status, connect or edit providers and installed agents." },
  keys: { tools: [], userControls: "Set or remove provider credentials. Agents must not request, read or print secret values." },
  mode: { tools: ["plan_enter", "plan_exit"], userControls: "Select the next run's mode. Plan hand-offs are available only when those tools are offered." },
  edges: { tools: ["graph.find_symbols", "graph.definition", "graph.callers_of", "graph.callees_of", "graph.neighbors", "graph.dependencies", "graph.blast_radius", "graph.tests_covering", "graph.assert_edge"], userControls: "Explore the graph with search, filters, physics, selection and source opening. Traverse small neighbourhoods; never inject the whole graph." },
} satisfies Record<DesktopView, { tools: string[]; userControls: string }>;

export const DESKTOP_VIEW_TOKENS = NAV_GROUPS.flatMap(({ views }) => views.map(([view]) => view));

export function featureCatalog() {
  return {
    availability: "This is a feature map, not a permission grant. Use only tools advertised for the current run and respect their schemas, mode, repository scope and approvals. Navigation opens a page; it does not execute its controls.",
    pages: NAV_GROUPS.flatMap(({ group, views }) => views.map(([view, label, description]) => ({ group, view, label, description, ...recipes[view] }))),
    interactiveAnswers: { tools: ["generateSandboxedUi"], recipe: "Render a useful chart, diagram or interactive explanation as an isolated widget, with source kept inside tool arguments." },
    generatedTools: { tools: ["desktop.manageTools", "desktop.defineTool", "desktop.executeTool"], recipe: "List saved helpers; define a desktop.custom name with parameters, language, functionBody and declared capabilities; execute by name and args. Prefer Python in bounded Monty with host_call only for declared connected desktop tools. No direct filesystem, shell, network or native APIs." },
    codeChanges: { tools: ["workspace.search", "workspace.read_file", "workspace.write_file", "workspace.edit_file", "git.diff", "git.apply_patch", "shell.run", "repository.test"], recipe: "Use only when offered for the selected mode. Inspect before editing, preserve unrelated work, run focused verification and report real results. The private desktop MCP bridge exposes read/search; an installed agent owns its other coding tools." },
    discovery: { tools: ["skills.search"], recipe: "Search by task for bounded tool, skill and command cards; open a returned skill's procedure. Cards do not install or authorise a tool." },
  };
}
