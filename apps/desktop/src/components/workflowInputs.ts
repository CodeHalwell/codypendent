import type { WorkflowCatalog } from "@codypendent/protocol";
import type { DesktopTransport } from "../transport.js";

export type WorkflowCatalogEntry = WorkflowCatalog["items"][number];
type InputSpecification = NonNullable<WorkflowCatalogEntry["inputs"][string]>;
export const MAX_WORKFLOW_INPUT_BYTES = 65_536;
const bytes = (text: string): number => new TextEncoder().encode(text).length;

/** App-owned memory survives view navigation without persisting input values to disk. */
export interface WorkflowLaunchMemory {
  transport: DesktopTransport | null;
  keys: Map<string, string>;
  editors: Map<string, { query: string; selected: string | null; drafts: Map<string, Map<string, string>> }>;
  manual: Map<string, { workflowId: string; inputs: string }>;
  runs: Map<string, string>;
}

export function createWorkflowLaunchMemory(): WorkflowLaunchMemory {
  return { transport: null, keys: new Map(), editors: new Map(), manual: new Map(), runs: new Map() };
}

/** A transport change cannot inherit another runtime owner's unresolved launches. */
export function bindWorkflowLaunchMemory(memory: WorkflowLaunchMemory, transport: DesktopTransport | null): void {
  if (memory.transport === transport) return;
  memory.transport = transport;
  memory.keys.clear(); memory.editors.clear(); memory.manual.clear(); memory.runs.clear();
}

export function workflowInputEntries(entry: WorkflowCatalogEntry): Array<[string, InputSpecification]> {
  return Object.entries(entry.inputs).filter((input): input is [string, InputSpecification] => input[1] !== undefined);
}

function canonicalValue(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonicalValue);
  if (value !== null && typeof value === "object") {
    const object = value as Record<string, unknown>;
    return Object.fromEntries(Object.keys(object).sort().map(key => [key, canonicalValue(object[key])]));
  }
  if (typeof value === "number" && !Number.isFinite(value)) throw new Error("JSON numbers must be finite.");
  return value;
}

/** Input types are descriptive names from the definition, rather than JSON Schemas. */
export function parseWorkflowInputs(entry: WorkflowCatalogEntry, drafts: ReadonlyMap<string, string>): {
  inputs: Record<string, unknown>;
  errors: Map<string, string>;
} {
  const values: Array<[string, unknown]> = [];
  const errors = new Map<string, string>();
  for (const [name, specification] of workflowInputEntries(entry)) {
    const raw = drafts.get(name) ?? "";
    if (raw.length > MAX_WORKFLOW_INPUT_BYTES || bytes(raw) > MAX_WORKFLOW_INPUT_BYTES) {
      errors.set(name, `${name} exceeds the 65,536-byte JSON input limit.`);
      continue;
    }
    const text = raw.trim();
    if (!text) {
      if (specification.required) errors.set(name, `${name} is required. Enter a JSON value.`);
      continue;
    }
    let value: unknown;
    try { value = JSON.parse(text); }
    catch { errors.set(name, `${name} must be valid JSON. Put text in double quotes.`); continue; }
    try { JSON.stringify(canonicalValue(value)); }
    catch { errors.set(name, `${name} must use finite numbers and JSON that can be serialized.`); continue; }
    values.push([name, value]);
  }
  const inputs = Object.fromEntries(values);
  if (!errors.size && bytes(JSON.stringify(inputs)) > MAX_WORKFLOW_INPUT_BYTES) {
    errors.set(values[0][0], "Combined workflow inputs exceed the 65,536-byte limit. Shorten the input values.");
  }
  return { inputs, errors };
}

/** Formatting and object-key order do not turn a retry into a new launch intent. */
export function workflowIntent(workflowId: string, definitionDigest: string, inputs: Record<string, unknown>): string {
  return JSON.stringify([workflowId, definitionDigest, canonicalValue(inputs)]);
}

/** UUIDs are local request identities, never credentials or a claim that a run started. */
export function newWorkflowRequestKey(): string {
  if (typeof crypto.randomUUID === "function") return crypto.randomUUID();
  return Array.from(crypto.getRandomValues(new Uint8Array(16)), byte => byte.toString(16).padStart(2, "0")).join("");
}
