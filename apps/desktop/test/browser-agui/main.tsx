import { useState } from "react";
import { createRoot } from "react-dom/client";
import { GeneratedToolResult } from "../../src/agui/GeneratedToolResult.js";
import { executeGeneratedTool } from "../../src/agui/sandbox.js";
import { createHostCall, type HostBindings } from "../../src/agui/hostTools.js";
import type { DesktopToolCall } from "../../src/agui/types.js";
import "../../src/theme.css";

const widget = { type: "open-generative-ui", initialHeight: 180, css: "body{background:#f7f5ef;color:#163b37}button{padding:10px;border-radius:8px;border:1px solid currentColor;cursor:pointer}", html: '<h2>Interactive project overview</h2><p>Counter: <strong id="count">0</strong></p><button id="increment">Add one</button>', jsFunctions: 'let count=0;document.getElementById("increment").onclick=()=>{document.getElementById("count").textContent=String(++count)};', jsExpressions: [] };
const bytes = new TextEncoder().encode(JSON.stringify(widget));
const context = { view: "sessions", repository: "/synthetic/project", title: "Isolated acceptance" };
const unavailable = async () => { throw new Error("Unavailable in this isolated check."); };
const host: HostBindings = { context: () => context, draft: () => "", navigate: () => {}, focusComposer: () => {}, setDraft: () => {}, setAppearance: () => {}, searchChats: unavailable, listModels: unavailable, listProviders: unavailable, listSkills: unavailable, listDocuments: unavailable, listMemory: unavailable, openFile: unavailable };
function Check() {
  const [result, setResult] = useState("Ready");
  const [busy, setBusy] = useState(false);
  const run = async (body: string) => {
    setBusy(true);
    setResult("Running…");
    const call: DesktopToolCall = { callId: crypto.randomUUID(), sessionId: "test", runId: "test", name: "desktop.custom.example", args: {}, expiresAt: Date.now() + 30000, definition: { name: "desktop.custom.example", description: "Acceptance check", parameters: { type: "object" }, functionBody: body, capabilities: ["desktop.getWorkspace"], version: 1 } };
    try { setResult(JSON.stringify(await executeGeneratedTool(call, createHostCall(() => host), new AbortController().signal))); }
    catch (error) { setResult(String(error)); }
    finally { setBusy(false); }
  };
  return <main style={{ maxWidth: 900, padding: 32, margin: "auto", color: "var(--cody-text)" }}><h1>Isolated agent tool acceptance</h1><p>Production renderer and sandbox. Synthetic context; no provider calls.</p>
    <GeneratedToolResult artifact={{ id: "fixture", sha256: "fixture", byte_length: bytes.length, media_type: "application/vnd.codypendent.desktop-tool+json", sensitivity: "Public" } as never} read={async () => bytes} />
    <div style={{ display: "flex", flexWrap: "wrap", gap: 8 }}>
      <button disabled={busy} onClick={() => void run('return await host.call("desktop.getWorkspace",{});')}>Run declared capability</button>
      <button disabled={busy} onClick={() => void run('return await host.call("desktop.openFile",{path:"secret.txt"});')}>Try undeclared capability</button>
      <button disabled={busy} onClick={() => void run('while(true){}')}>Check CPU timeout</button>
      <button disabled={busy} onClick={() => void run('let network;try{await fetch("https://example.com");network="unexpected"}catch{network="blocked"}return {network,window:typeof window,native:typeof __TAURI_INTERNALS__};')}>Check isolation</button>
    </div><pre role="status" style={{ whiteSpace: "pre-wrap", marginTop: 24 }}>{result}</pre></main>;
}
createRoot(document.getElementById("root")!).render(<Check />);
