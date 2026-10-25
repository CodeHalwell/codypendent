import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { ArtifactRef } from "@codypendent/protocol";
import { ArtifactSaveButton, type ReadArtifact, type SaveArtifact } from "../components/ArtifactActions.js";
import { sandboxHandshake, sandboxUrl } from "./sandbox.js";
import type { GeneratedUi } from "./types.js";
import { resultSummary } from "../components/toolPresentation.js";

export function generatedUi(value: unknown): GeneratedUi | null {
  if (value === null || typeof value !== "object" || !("type" in value) || value.type !== "open-generative-ui") return null;
  const ui = value as Record<string, unknown>;
  if (typeof ui.html !== "string") throw new Error("Interactive result HTML must be text.");
  for (const field of ["css", "jsFunctions"]) {
    if (ui[field] !== undefined && typeof ui[field] !== "string") throw new Error(`Interactive result ${field} must be text.`);
  }
  for (const field of ["jsExpressions", "placeholderMessages"]) {
    if (ui[field] !== undefined && (!Array.isArray(ui[field]) || !ui[field].every((item: unknown) => typeof item === "string"))) {
      throw new Error(`Interactive result ${field} must be a list of text values.`);
    }
  }
  if (ui.initialHeight !== undefined && (typeof ui.initialHeight !== "number" || !Number.isFinite(ui.initialHeight))) {
    throw new Error("Interactive result height must be a finite number.");
  }
  return ui as GeneratedUi;
}

type Result = { key: string; value?: unknown; widget?: GeneratedUi | null; error?: string };

export function GeneratedToolResult({ artifact, read, save }: { artifact: ArtifactRef; read?: ReadArtifact; save?: SaveArtifact }) {
  const [attempt, setAttempt] = useState(0);
  const [result, setResult] = useState<Result | null>(null);
  const key = `${artifact.id}:${artifact.sha256}:${artifact.byte_length}:${attempt}`;
  useEffect(() => {
    let live = true;
    setResult({ key });
    if (!read) { setResult({ key, error: "Reconnect to load this result." }); return; }
    if (artifact.byte_length > 262144) { setResult({ key, error: "This result is too large to preview. Save it to read the complete file." }); return; }
    void read(artifact).then((bytes) => {
      if (bytes.byteLength > 262144) throw new Error("This result is too large to preview.");
      const value: unknown = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
      const widget = generatedUi(value);
      if (live) setResult({ key, value, widget });
    }).catch((reason) => { if (live) setResult({ key, error: String(reason) }); });
    return () => { live = false; };
  }, [key, read]);
  const current = result?.key === key ? result : null;
  return <div className="agent-tool-result">
    {current?.error ? <div role="alert">{current.error} <button onClick={() => setAttempt((value) => value + 1)}>Retry</button></div>
      : current?.value === undefined ? <div role="status">Loading result…</div>
      : current.widget ? <GeneratedWidget key={key} widget={current.widget} />
      : <><p className="agent-tool-result-summary">{resultSummary(current.value)}</p><details><summary>Technical details</summary><pre>{JSON.stringify(current.value, null, 2)}</pre></details></>}
    <ArtifactSaveButton artifact={artifact} onSave={save} />
  </div>;
}

function GeneratedWidget({ widget }: { widget: GeneratedUi }) {
  const [attempt, setAttempt] = useState(0);
  const token = useMemo(() => crypto.randomUUID(), [widget, attempt]);
  const url = useMemo(() => sandboxUrl({ token, widget }), [token, widget]);
  const frame = useRef<HTMLIFrameElement>(null);
  const [height, setHeight] = useState(Math.min(1200, Math.max(120, widget.initialHeight ?? 320)));
  const [error, setError] = useState<string | null>(null);
  const [ready, setReady] = useState(false);
  useLayoutEffect(() => {
    setReady(false);
    setError(null);
    const handshake = sandboxHandshake(frame.current!, { token, widget });
    const timer = setTimeout(() => setError("The interactive result could not open."), 5000);
    const receive = (event: MessageEvent) => {
      if (handshake(event)) return;
      if (event.source !== frame.current?.contentWindow || event.data?.token !== token) return;
      if (event.data.kind === "ready") { clearTimeout(timer); setReady(true); }
      if (event.data.kind === "height" && Number.isFinite(event.data.value)) setHeight(Math.min(1200, Math.max(120, event.data.value)));
      if (event.data.kind === "error") { clearTimeout(timer); setError(String(event.data.value).slice(0, 500)); }
    };
    window.addEventListener("message", receive);
    return () => { clearTimeout(timer); window.removeEventListener("message", receive); };
  }, [token, widget]);
  return <div>
    {!ready && !error && <div role="status">Opening interactive result…</div>}
    <iframe key={token} ref={frame} src={url} sandbox="allow-scripts" referrerPolicy="no-referrer" title="Interactive agent result" style={{ width: "100%", height, border: 0, opacity: ready ? 1 : 0, background: "var(--cody-panel)", colorScheme: "normal" }} />
    {error && <div role="alert">{error} <button onClick={() => setAttempt((value) => value + 1)}>Retry</button></div>}
  </div>;
}
