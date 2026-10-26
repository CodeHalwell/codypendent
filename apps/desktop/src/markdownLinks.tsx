import { useState, type CSSProperties } from "react";
import { sanitizeFailureText } from "./failure.js";

export interface MarkdownOptions {
  onOpenSource?: (path: string, line?: number) => Promise<void>;
}

export interface SourceReference {
  path: string;
  line?: number;
}

export function sourceReference(destination: string): SourceReference | undefined {
  let target = destination.trim();
  if (target.startsWith("<") && target.endsWith(">")) target = target.slice(1, -1);
  try { target = decodeURIComponent(target); } catch { return undefined; }
  if (!target || target.length > 4096 || /[\u0000-\u001f\u007f\\]/.test(target)
    || target.startsWith("//") || target.startsWith("~")) return undefined;
  const suffix = /(?::([1-9]\d*)(?::[1-9]\d*)?|#L([1-9]\d*)(?:-L[1-9]\d*)?)$/.exec(target);
  const line = suffix ? Number(suffix[1] ?? suffix[2]) : undefined;
  if (line !== undefined && (!Number.isSafeInteger(line) || line > 0xffff_ffff)) return undefined;
  const path = (suffix ? target.slice(0, suffix.index) : target).replace(/^\.\//, "");
  if (!path || /^[a-z][a-z\d+.-]*:/i.test(path) || /[<>#?\"|]/.test(path) || path.split("/").some((part) => part === "." || part === "..")
    || (!path.includes("/") && !/\.[\w-]+$|^(?:README|LICENSE|Dockerfile|Makefile|Justfile)$/i.test(path))) return undefined;
  return { path, ...(line === undefined ? {} : { line }) };
}

function visibleLabel(label: string, reference?: SourceReference): string {
  const short = label.trim();
  if (!reference || (!short.includes("/") && short.length <= 64)) return short;
  const name = reference.path.split("/").at(-1) ?? short;
  return reference.line ? `${name}:${reference.line}` : name;
}

const LINK: CSSProperties = { color: "var(--cody-link)", overflowWrap: "anywhere" };
const SOURCE: CSSProperties = {
  ...LINK, display: "inline", font: "inherit", lineHeight: "inherit", background: "none",
  border: 0, padding: 0, textDecoration: "underline", textDecorationColor: "var(--cody-border-strong)",
  textUnderlineOffset: "3px", cursor: "pointer", textAlign: "left",
};

export function MarkdownLink({ label, destination, onOpenSource }: MarkdownOptions & { label: string; destination: string }) {
  const [opening, setOpening] = useState(false);
  const [error, setError] = useState<string>();
  const reference = sourceReference(destination);
  const text = visibleLabel(label, reference);
  if (!reference || !onOpenSource) return <span style={LINK} title={destination}>{text}{!reference && /^https?:\/\//i.test(destination) ? ` (${destination})` : ""}</span>;
  const open = async () => {
    if (opening) return;
    setOpening(true); setError(undefined);
    try { await onOpenSource(reference.path, reference.line); }
    catch (failure) {
      const message = failure instanceof Error ? failure.message : String(failure);
      setError(/run.s worktree is no longer available/i.test(message)
        ? "The files from this earlier run are no longer available."
        : sanitizeFailureText(message));
    }
    finally { setOpening(false); }
  };
  return <>
    <button type="button" className="markdown-source-reference" style={SOURCE} title={destination}
      aria-label={`Open ${text}${reference.line && !text.endsWith(`:${reference.line}`) ? ` at line ${reference.line}` : ""}`}
      aria-busy={opening || undefined} disabled={opening} onClick={() => void open()}>{text}</button>
    {error && <span role="alert" style={{ color: "var(--cody-danger-text)", fontSize: "0.9em" }}> Could not open source: {error}</span>}
  </>;
}
