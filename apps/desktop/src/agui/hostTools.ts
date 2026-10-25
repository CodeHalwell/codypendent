import { boundedJson } from "./sandbox.js";
import type { DesktopContext, DesktopTool, HostCall, Json } from "./types.js";
import { DESKTOP_VIEW_TOKENS, featureCatalog } from "./featureCatalog.js";

type Schema = Record<string, unknown>;
export type HostBindings = {
  context: () => DesktopContext;
  navigate: (view: string) => void;
  focusComposer: () => void;
  draft: () => string;
  setDraft: (text: string, expected: string) => void;
  setAppearance: (appearance: string) => void;
  searchChats: (query: string) => Promise<unknown>;
  listModels: () => Promise<unknown>;
  listProviders: () => Promise<unknown>;
  listSkills: () => Promise<unknown>;
  listDocuments: () => Promise<unknown>;
  listMemory: () => Promise<unknown>;
  openFile: (path: string, line?: number) => Promise<void>;
};

const string = (maxLength = 4096): Schema => ({ type: "string", maxLength });
const commands: Record<string, { description: string; properties?: Record<string, Schema>; required?: string[]; execute: (host: HostBindings, args: Record<string, Json>) => unknown }> = {
  getWorkspace: { description: "Read the current desktop view, repository, model and run context. No credentials or draft contents.", execute: (host) => host.context() },
  listFeatures: { description: "Discover every Codypendent workspace page, its exact navigation token, available feature families and tool recipes. Distinguishes agent tools from controls the user must operate. Tool names in this map are usable only when offered for the current run.", execute: () => featureCatalog() },
  navigate: { description: "Open a Codypendent workspace page using a listed view token. Use desktop.listFeatures to discover pages and which actions have tools; navigation itself does not execute page controls.", properties: { view: { ...string(40), enum: DESKTOP_VIEW_TOKENS } }, required: ["view"], execute: (host, args) => { host.navigate(String(args.view)); return { opened: args.view }; } },
  focusComposer: { description: "Return to the chat and focus its composer without sending a message.", execute: (host) => { host.focusComposer(); return { focused: true }; } },
  readDraft: { description: "Read the unsent composer draft when the user asks for help preparing a message.", execute: (host) => ({ draft: host.draft() }) },
  setDraft: { description: "Prepare an unsent message. Supply the exact expected current draft so concurrent user typing is preserved. This never sends a message.", properties: { text: string(65536), expectedDraft: string(65536) }, required: ["text", "expectedDraft"], execute: (host, args) => { host.setDraft(String(args.text), String(args.expectedDraft)); return { prepared: true }; } },
  setAppearance: { description: "Change desktop appearance to system, light or dark.", properties: { appearance: { type: "string", enum: ["system", "light", "dark"] } }, required: ["appearance"], execute: (host, args) => { host.setAppearance(String(args.appearance)); return { appearance: args.appearance }; } },
  searchChats: { description: "Search saved chats. Returns the real bounded first page and continuation cursor.", properties: { query: string(1024) }, required: ["query"], execute: (host, args) => host.searchChats(String(args.query)) },
  listModels: { description: "List configured model profiles and the current default.", execute: (host) => host.listModels() },
  listProviders: { description: "List available providers and installed agent adapters, without credentials.", execute: (host) => host.listProviders() },
  listSkills: { description: "Read the registered workspace skills and permissions.", execute: (host) => host.listSkills() },
  listDocuments: { description: "Read the current workspace document cards.", execute: (host) => host.listDocuments() },
  listMemory: { description: "Read curated memory cards for this workspace.", execute: (host) => host.listMemory() },
  openFile: { description: "Open a repository-relative file in the installed editor for the current run's checkout.", properties: { path: string(), line: { type: "integer", minimum: 1, maximum: 10000000 } }, required: ["path"], execute: async (host, args) => { await host.openFile(String(args.path), typeof args.line === "number" ? args.line : undefined); return { opened: args.path }; } },
};

export const HOST_TOOLS: DesktopTool[] = Object.entries(commands).map(([verb, command]) => ({
  name: `desktop.${verb}`, description: command.description, version: 1, capabilities: [],
  parameters: { type: "object", properties: command.properties ?? {}, required: command.required ?? [], additionalProperties: false },
}));

function validateArguments(command: typeof commands[string], args: Record<string, Json>) {
  for (const key of command.required ?? []) if (!(key in args)) throw new Error(`Missing argument: ${key}`);
  for (const [key, value] of Object.entries(args)) {
    const schema = command.properties?.[key];
    if (!schema) throw new Error(`Unexpected argument: ${key}`);
    if (schema.type === "string" && (typeof value !== "string" || value.length > Number(schema.maxLength ?? 65536))) throw new Error(`Invalid text argument: ${key}`);
    if (schema.type === "integer" && (typeof value !== "number" || !Number.isSafeInteger(value) || value < Number(schema.minimum) || value > Number(schema.maximum))) throw new Error(`Invalid line number: ${key}`);
    if (Array.isArray(schema.enum) && !schema.enum.includes(value)) throw new Error(`Invalid choice: ${key}`);
  }
}

export function createHostCall(bindings: () => HostBindings): HostCall {
  return async (name, args) => {
    const command = name.startsWith("desktop.") ? commands[name.slice(8)] : undefined;
    if (!command) throw new Error(`Unknown desktop capability: ${name}`);
    if (args === null || Array.isArray(args) || typeof args !== "object") throw new Error("Tool arguments must be an object.");
    validateArguments(command, args);
    return boundedJson(await command.execute(bindings(), args) ?? null);
  };
}
