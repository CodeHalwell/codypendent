import React, { useEffect, useRef, useState } from "react";
import type { ArtifactRef } from "@codypendent/protocol";
import { sanitizeFailureText } from "../failure.js";
import { useCommandFocus } from "../useCommandFocus.js";
import { PatchReview, type OpenReviewFile } from "./PatchReview.js";

export type SaveArtifact = (artifact: ArtifactRef) => Promise<string | null>;
export type ReadArtifact = (artifact: ArtifactRef) => Promise<Uint8Array>;

const button: React.CSSProperties = {
  padding: "5px 10px", border: "1px solid var(--cody-border-strong)", borderRadius: 6,
  background: "var(--cody-inset)", color: "var(--cody-text)", cursor: "pointer",
};
const describe = (error: unknown) => sanitizeFailureText(error instanceof Error ? error.message : String(error));
const pendingFeedback = { phase: "pending" } as const;

type SaveProps = { artifact: ArtifactRef; onSave?: SaveArtifact };
type PatchProps = { preview?: string; files?: string[]; runId?: string; artifact?: ArtifactRef; onRead?: ReadArtifact; onSave?: SaveArtifact; onOpenFile?: OpenReviewFile };

/** A replaced reference or source owns a new set of loaded bytes and notices. */
function useArtifactScope(artifact: ArtifactRef | undefined, source: ReadArtifact | SaveArtifact | undefined) {
  const key = artifact ? JSON.stringify([artifact.id, artifact.sha256, artifact.byte_length, artifact.media_type, artifact.sensitivity]) : "none";
  const [scope, setScope] = useState({ key, source, generation: 0 });
  if (scope.key !== key || scope.source !== source) {
    setScope({ key, source, generation: scope.generation + 1 });
  }
  return scope.generation;
}

function useLiveRequest() {
  const live = useRef(true);
  useEffect(() => {
    live.current = true;
    return () => { live.current = false; };
  }, []);
  return live;
}

/** Save through the shell, with cancellation distinct from a saved file. */
export function ArtifactSaveButton(props: SaveProps) {
  const scope = useArtifactScope(props.artifact, props.onSave);
  return <SaveAction key={scope} {...props} />;
}

function SaveAction({ artifact, onSave }: SaveProps) {
  const live = useLiveRequest();
  const pending = useRef(false);
  const [saving, setSaving] = useState(false);
  const { ref, preserveFocus } = useCommandFocus(saving ? pendingFeedback : undefined);
  const [result, setResult] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const save = async () => {
    if (!onSave || pending.current) return;
    preserveFocus();
    pending.current = true;
    setSaving(true);
    setError(null);
    setResult(null);
    try {
      const path = await onSave(artifact);
      if (!live.current) return;
      setResult(path === null ? "Save cancelled." : `Saved to ${path}`);
    } catch (error) {
      if (!live.current) return;
      setError(`Could not save: ${describe(error)}`);
    } finally {
      pending.current = false;
      if (live.current) setSaving(false);
    }
  };
  return <div ref={ref} tabIndex={-1} role="group" aria-label="Save artifact" aria-busy={saving}
    style={{ marginTop: 8, fontSize: 12, overflowWrap: "anywhere" }}>
    <button style={button} disabled={!onSave || saving} onClick={() => void save()}>
      {saving ? "Saving…" : "Save file…"}
    </button>
    {!onSave && <span style={{ marginLeft: 8 }}>Connect in the desktop app to save this file.</span>}
    {result && <div role="status" style={{ marginTop: 6 }}>{result}</div>}
    {error && <div role="alert" style={{ marginTop: 6, color: "var(--cody-danger-soft)" }}>{error} Try saving again.</div>}
  </div>;
}

/** Fetch on demand; a large artifact remains downloadable without filling the DOM. */
export function PatchContent(props: PatchProps) {
  const scope = useArtifactScope(props.artifact, props.onRead);
  return <PatchPreview key={scope} {...props} />;
}

function PatchPreview({ preview, files, runId, artifact, onRead, onSave, onOpenFile }: PatchProps) {
  const live = useLiveRequest();
  const [loaded, setLoaded] = useState<{ text: string; truncated: boolean } | null>(null);
  const [loading, setLoading] = useState(false);
  const { ref, preserveFocus } = useCommandFocus(loading ? pendingFeedback : undefined);
  const [error, setError] = useState<string | null>(null);
  const pending = useRef(false);
  const read = async () => {
    if (!artifact || !onRead || pending.current) return;
    preserveFocus();
    pending.current = true;
    setLoading(true);
    setError(null);
    try {
      const bytes = await onRead(artifact);
      if (!live.current) return;
      const limit = 1_000_000;
      const truncated = bytes.byteLength > limit;
      const decoder = new TextDecoder("utf-8", { fatal: true });
      // A truncated slice can end inside a code point; streaming retains that
      // incomplete tail instead of replacing it or rejecting a valid patch.
      const text = decoder.decode(bytes.subarray(0, limit), { stream: truncated });
      setLoaded({ text, truncated });
    } catch (error) {
      if (!live.current) return;
      setError(`Could not load the patch: ${describe(error)}`);
    } finally {
      pending.current = false;
      if (live.current) setLoading(false);
    }
  };
  return <div ref={ref} tabIndex={-1} role="group" aria-label="Patch content" aria-busy={loading}>
    <div style={{ fontSize: 12, color: "var(--cody-text-muted)" }}>
      {loaded ? (loaded.truncated ? "Large patch: showing the first 1 MB. Save the file for the complete patch." : "Full patch") : "Patch preview"}
    </div>
    <PatchReview source={loaded?.text ?? preview} files={files} runId={runId} label={loaded ? "Loaded patch" : "Patch preview"} onOpenFile={onOpenFile} />
    {artifact && <>
      <button style={button} disabled={!onRead || loading} onClick={() => void read()}>
        {loading ? "Loading patch…" : loaded ? "Reload patch" : "Load full patch"}
      </button>
      {error && <div role="alert" style={{ marginTop: 6, color: "var(--cody-danger-soft)" }}>{error} Try loading again.</div>}
      <ArtifactSaveButton artifact={artifact} onSave={onSave} />
    </>}
  </div>;
}
