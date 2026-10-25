export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export type DesktopContext = Record<string, string | number | boolean | null>;
export type DesktopTool = {
  name: string;
  description: string;
  parameters: Record<string, unknown>;
  functionBody?: string;
  language?: "javascript" | "python";
  capabilities: string[];
  version: number;
};
export type DesktopManifest = { context: DesktopContext; tools: DesktopTool[] };
export type DesktopToolCall = {
  callId: string;
  sessionId: string;
  runId: string;
  name: string;
  args: Json;
  definition: DesktopTool;
  expiresAt: number;
};
export type HostCall = (name: string, args: Json) => Promise<Json>;
export type GeneratedUi = {
  type: "open-generative-ui";
  initialHeight?: number;
  placeholderMessages?: string[];
  css?: string;
  html: string;
  jsFunctions?: string;
  jsExpressions?: string[];
};

let manifest: DesktopManifest | null = null;
export function setDesktopManifest(value: DesktopManifest | null) { manifest = value; }
export function desktopManifest() { return manifest; }
