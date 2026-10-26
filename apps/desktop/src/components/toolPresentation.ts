const TOOL_TITLES: Record<string, string> = {
  "graph.find_symbols": "Find code symbols",
  "graph.definition": "Find a definition",
  "graph.callers_of": "Find callers",
  "graph.callees_of": "Follow function calls",
  "graph.neighbors": "Explore related code",
  "graph.dependencies": "Explore dependencies",
  "graph.blast_radius": "Find affected code",
  "graph.tests_covering": "Find related tests",
  generateSandboxedUi: "Create an interactive answer",
  "desktop.defineTool": "Create a reusable helper",
  "desktop.manageTools": "Manage saved helpers",
  "desktop.executeTool": "Run a saved helper",
  "desktop.getWorkspace": "Read workspace context",
  "desktop.navigate": "Open a workspace page",
  "desktop.focusComposer": "Return to chat",
  "desktop.readDraft": "Read the current draft",
  "desktop.setDraft": "Prepare a message",
  "desktop.setAppearance": "Change appearance",
  "desktop.searchChats": "Find a chat",
  "desktop.listModels": "List available models",
  "desktop.listProviders": "List connected providers",
  "desktop.listSkills": "List workspace skills",
  "desktop.listDocuments": "List workspace documents",
  "desktop.listMemory": "Read workspace memory",
  "desktop.openFile": "Open a project file",
};

// Exact aliases emitted by the native tool-name codec, including its digest.
const TOOL_ALIASES: Record<string, string> = {
  cpd_graph_find_symbols_f5044150b32d1f48e89c: "graph.find_symbols",
  cpd_graph_definition_710fe97a080bb58301d5: "graph.definition",
  cpd_graph_callers_of_368c07f2364600495d82: "graph.callers_of",
  cpd_graph_callees_of_431b0ce68dfc014962a3: "graph.callees_of",
  cpd_graph_neighbors_7438d95b0c33c56b2fb3: "graph.neighbors",
  cpd_graph_dependencies_d570783c09ab9682ac4c: "graph.dependencies",
  cpd_graph_blast_radius_9ca7a4c4b08c73820a92: "graph.blast_radius",
  cpd_graph_tests_covering_4706c1c7959f73fdea44: "graph.tests_covering",
  cpd_desktop_defineTool_2749a2214202cee72b57: "desktop.defineTool",
  cpd_desktop_manageTools_dfd7268e213d05d3c5d7: "desktop.manageTools",
  cpd_desktop_executeTool_0673826a29d7faf9af07: "desktop.executeTool",
  cpd_desktop_getWorkspace_22c0dece2cf9685a8d49: "desktop.getWorkspace",
  cpd_desktop_navigate_56f488c931d9a76700a2: "desktop.navigate",
  cpd_desktop_focusComposer_299ceecf05cf5330063b: "desktop.focusComposer",
  cpd_desktop_readDraft_2e86750db3ff12df8d07: "desktop.readDraft",
  cpd_desktop_setDraft_94050c642746423ba21b: "desktop.setDraft",
  cpd_desktop_setAppearance_c63c347bdb41e6608c84: "desktop.setAppearance",
  cpd_desktop_searchChats_616a98ce0ed7a78a5add: "desktop.searchChats",
  cpd_desktop_listModels_0551912061b830aac49d: "desktop.listModels",
  cpd_desktop_listProviders_365dec660a135bb624fb: "desktop.listProviders",
  cpd_desktop_listSkills_7d99fe677b04dff17e42: "desktop.listSkills",
  cpd_desktop_listDocuments_3e37c9593f4c5794f544: "desktop.listDocuments",
  cpd_desktop_listMemory_55697f434423e66d55e8: "desktop.listMemory",
  cpd_desktop_openFile_1a4aaa649e02b4ebe8e7: "desktop.openFile",
};

export function desktopToolName(name: string): string | null {
  const local = name.replace(/^(mcp\.codypendent-desktop\.|mcp__codypendent-desktop__)/, "");
  const canonical = TOOL_ALIASES[local] ?? local;
  return TOOL_TITLES[canonical] || canonical.startsWith("desktop.custom.") ? canonical : null;
}

export function toolTitle(name: string): string {
  name = desktopToolName(name) ?? name;
  if (TOOL_TITLES[name]) return TOOL_TITLES[name];
  if (!name.startsWith("desktop.custom.")) return name;
  const title = name.slice("desktop.custom.".length).replace(/([a-z])([A-Z])/g, "$1 $2").replace(/[_-]/g, " ");
  return title.charAt(0).toUpperCase() + title.slice(1);
}

/** Recognise complete transport data only; prose and fenced code stay intact. */
export function jsonData(text: string): unknown | undefined {
  if (!/^[\s]*[\[{]/.test(text)) return undefined;
  try { return JSON.parse(text); } catch { return undefined; }
}

export function resultSummary(value: unknown): string {
  if (value === null) return "Completed with no additional output.";
  if (typeof value === "string") return value.length <= 240 && jsonData(value) === undefined ? value : "Result ready.";
  if (typeof value === "number" || typeof value === "boolean") return `Result: ${value}`;
  if (Array.isArray(value)) return `${value.length} result${value.length === 1 ? "" : "s"} returned.`;
  if (typeof value !== "object") return "Result ready.";
  const row = value as Record<string, unknown>;
  if (row.registered === true && typeof row.name === "string") return `Saved ${toolTitle(row.name)} for this chat.`;
  if (row.deleted === true && typeof row.name === "string") return `Removed ${toolTitle(row.name)} from this chat.`;
  if (row.prepared === true) return "Your draft is ready to review.";
  if (row.focused === true) return "The message box is ready.";
  if (typeof row.message === "string" && row.message.length <= 240 && jsonData(row.message) === undefined) return row.message;
  if (typeof row.summary === "string" && row.summary.length <= 240 && jsonData(row.summary) === undefined) return row.summary;
  return "Result ready.";
}

/** Older ACP adapters recorded this provider envelope as a warning note. */
export function legacyProviderError(text: string): string | null {
  let index = -1;
  for (let candidate = 0; candidate < 32; candidate++) {
    index = text.indexOf("{", index + 1);
    if (index < 0) return null;
    try {
      const value: unknown = JSON.parse(text.slice(index).trim());
      if (!value || typeof value !== "object" || Array.isArray(value)) continue;
      const row = value as Record<string, unknown>;
      if (row.type !== "error" || typeof row.status !== "number" || !Number.isInteger(row.status) || row.status < 400 || row.status > 599) continue;
      if (!row.error || typeof row.error !== "object" || Array.isArray(row.error)) continue;
      const error = row.error as Record<string, unknown>;
      if (typeof error.type === "string" && typeof error.message === "string" && error.message.trim() && jsonData(error.message) === undefined) return error.message;
    } catch { /* A warning prefix or nested object can precede the envelope. */ }
  }
  return null;
}
