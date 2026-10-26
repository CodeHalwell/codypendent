import { useEffect, useMemo, useRef, useState } from "react";
import { isReviewablePath, MAX_DISPLAY_LINE, parsePatchReview, REVIEW_PAGE_LINES, type ReviewFile } from "../patchReview.js";
import { sanitizeFailureText } from "../failure.js";
import { useCommandFocus } from "../useCommandFocus.js";
import "./patchReview.css";

export type OpenReviewFile = (path: string, line?: number, runId?: string) => Promise<void>;
interface Props { source?: string; label: string; files?: string[]; runId?: string; onOpenFile?: OpenReviewFile }
const RAW_LIMIT = 32_000;
const SOURCE_PAGE = 8_000;
const pendingFeedback = { phase: "pending" } as const;

/** Only the selected file/hunk/page is mounted, even for very large patches. */
export function PatchReview({ source = "", label, files = [], runId, onOpenFile }: Props) {
  const parsed = useMemo(() => parsePatchReview(source), [source]);
  const [scope, setScope] = useState({ source, runId, generation: 0 });
  if (scope.source !== source || scope.runId !== runId) setScope({ source, runId, generation: scope.generation + 1 });
  // A changed source owns fresh navigation; a loaded full artifact must not
  // inherit an index that referred to a different preview file or hunk.
  return <Review key={scope.generation} parsed={parsed} source={source} label={label} files={files} runId={runId} onOpenFile={onOpenFile} />;
}

function Review({ parsed, source, label, files, runId, onOpenFile }: Props & { source: string; parsed: ReturnType<typeof parsePatchReview> }) {
  const [fileIndex, setFileIndex] = useState(0);
  const [hunkIndex, setHunkIndex] = useState(0);
  const [page, setPage] = useState(0);
  const [opening, setOpening] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { ref, preserveFocus } = useCommandFocus(opening ? pendingFeedback : undefined);
  const selectionVersion = useRef(0);
  const live = useRef(true);
  const pending = useRef(false);
  useEffect(() => { live.current = true; return () => { live.current = false; }; }, []);
  const currentOpen = useRef(onOpenFile);
  currentOpen.current = onOpenFile;
  useEffect(() => { setError(null); }, [onOpenFile]);
  const file = parsed.files[fileIndex];
  const hunk = file?.hunks[hunkIndex];
  const rows = hunk?.lines.slice(page * REVIEW_PAGE_LINES, (page + 1) * REVIEW_PAGE_LINES) ?? [];
  const extraFiles = useMemo(() => Array.from(new Set(files)).filter((path) => !parsed.files.some((item) => item.path === path)).slice(0, 100), [files, parsed.files]);
  const selectFile = (index: number) => { selectionVersion.current++; setFileIndex(index); setHunkIndex(0); setPage(0); setError(null); };
  const open = async (path: string, line = 1) => {
    if (!onOpenFile || !isReviewablePath(path) || pending.current) return;
    const source = onOpenFile;
    const selection = selectionVersion.current;
    preserveFocus();
    pending.current = true; setOpening(true); setError(null);
    try {
      if (runId) await source(path, Math.max(1, line), runId);
      else await source(path, Math.max(1, line));
    }
    catch (error) {
      if (live.current && currentOpen.current === source && selection === selectionVersion.current) setError(sanitizeFailureText(error instanceof Error ? error.message : String(error)));
    } finally { pending.current = false; if (live.current) setOpening(false); }
  };
  const previous = hunkIndex > 0 || fileIndex > 0;
  const next = Boolean(file && (hunkIndex + 1 < file.hunks.length || fileIndex + 1 < parsed.files.length));
  const moveChange = (direction: -1 | 1) => {
    selectionVersion.current++;
    if (direction === -1) {
      if (hunkIndex > 0) setHunkIndex(hunkIndex - 1);
      else if (fileIndex > 0) { setFileIndex(fileIndex - 1); setHunkIndex(Math.max(0, parsed.files[fileIndex - 1]!.hunks.length - 1)); }
    } else {
      if (file && hunkIndex + 1 < file.hunks.length) setHunkIndex(hunkIndex + 1);
      else if (fileIndex + 1 < parsed.files.length) { setFileIndex(fileIndex + 1); setHunkIndex(0); }
    }
    setPage(0); setError(null);
  };
  const firstLine = (item: ReviewFile) => Math.max(1, item.newPath === "/dev/null" ? item.hunks[0]?.oldStart ?? 1 : item.hunks[0]?.newStart ?? 1);
  const lineLink = (number: number | undefined, target: number | undefined, removed: boolean) => (
    number !== undefined && target !== undefined && onOpenFile && file
      ? <button type="button" disabled={opening} className="patch-review-line-link"
          aria-label={`Open ${file.path} at line ${target}${removed && file.status !== "deleted" ? " (nearest current line)" : ""}`}
          title={removed && file.status !== "deleted" ? "Open the nearest line in the current file" : "Open this line in your editor"}
          onClick={() => void open(file.path, target)}>{number}</button>
      : number ?? ""
  );
  return <div ref={ref} className="patch-review" role="region" tabIndex={-1} aria-label="Change review" aria-busy={opening}>
    {(parsed.files.length > 0 || extraFiles.length > 0) && <nav className="patch-review-files" aria-label="Changed files">
      {parsed.files.map((item, index) => <button key={`${item.path}:${index}`} type="button" aria-current={index === fileIndex ? "true" : undefined}
        onClick={() => selectFile(index)} title={item.path}>
        <span>{item.path}</span><small>{item.status} · +{item.additions} −{item.deletions}</small>
      </button>)}
      {extraFiles.map((path) => <button key={path} type="button" disabled={!onOpenFile || !isReviewablePath(path) || opening}
        onClick={() => void open(path)} title={isReviewablePath(path) ? `Open ${path} in editor` : "This path cannot be opened"}>
        <span>{path}</span><small>Open in editor</small>
      </button>)}
    </nav>}
    {parsed.notice && source && <p className="patch-review-notice">{parsed.notice}. {parsed.files.length > 0 ? "Only complete changes are shown below." : "The available text is shown below."}</p>}
    {file && <>
      <div className="patch-review-toolbar">
        <div className="patch-review-heading"><strong>{file.path}</strong><span>{file.status}{file.status === "renamed" ? ` from ${file.oldPath}` : ""}</span></div>
        {onOpenFile && <button type="button" disabled={opening} onClick={() => void open(file.path, firstLine(file))}
          title={file.status === "deleted" ? "Open the original path if it still exists in this workspace" : `Open ${file.path} in editor`}>
          {opening ? "Opening…" : "Open in editor"}
        </button>}
      </div>
      <div className="patch-review-controls">
          <button type="button" aria-label="Previous change" disabled={!previous} onClick={() => moveChange(-1)}>← Previous</button>
          {file.hunks.length > 0 ? <label>Change <select aria-label="Change in file" value={hunkIndex} onChange={(event) => { setHunkIndex(Number(event.target.value)); setPage(0); }}>
            {file.hunks.map((item, index) => <option key={index} value={index}>{index + 1} of {file.hunks.length} · line {Math.max(1, file.newPath === "/dev/null" ? item.oldStart : item.newStart)}</option>)}
          </select></label> : <span>{file.status === "binary" ? "Binary content cannot be shown as text." : "File metadata changed; no text hunks."}</span>}
          <button type="button" aria-label="Next change" disabled={!next} onClick={() => moveChange(1)}>Next →</button>
        </div>
      {file.hunks.length > 0 && <>
        <div className="patch-review-table-scroll" tabIndex={0} role="region" aria-label={`Diff for ${file.path}`}>
          <table className="patch-review-table">
            <caption>{hunk?.header}</caption>
            <thead><tr><th scope="col">Before</th><th scope="col">After</th><th scope="col" aria-label="Change">+/−</th><th scope="col">Code</th></tr></thead>
            <tbody>{rows.map((row, index) => <tr key={page * REVIEW_PAGE_LINES + index} data-kind={row.kind}>
              <td>{row.newLine === undefined ? lineLink(row.oldLine, row.targetLine, row.kind === "deletion") : row.oldLine ?? ""}</td>
              <td>{lineLink(row.newLine, row.targetLine, false)}</td>
              <td aria-label={row.kind}>{row.kind === "addition" ? "+" : row.kind === "deletion" ? "−" : row.kind === "note" ? "\\" : " "}</td>
              <td><code>{row.text.slice(0, MAX_DISPLAY_LINE) || " "}</code>
                {row.text.length > MAX_DISPLAY_LINE && <span className="patch-review-line-limit">… long line clipped; save patch for full text</span>}
              </td>
            </tr>)}</tbody>
          </table>
        </div>
        {hunk && hunk.lines.length > REVIEW_PAGE_LINES && <div className="patch-review-controls">
          <button type="button" disabled={page === 0} onClick={() => setPage(page - 1)}>Previous lines</button>
          <span role="status">Lines {page * REVIEW_PAGE_LINES + 1}–{Math.min((page + 1) * REVIEW_PAGE_LINES, hunk.lines.length)} of {hunk.lines.length}</span>
          <button type="button" disabled={(page + 1) * REVIEW_PAGE_LINES >= hunk.lines.length} onClick={() => setPage(page + 1)}>Next lines</button>
        </div>}
      </>}
    </>}
    {error && <p role="alert" className="patch-review-error">Could not open file: {error}</p>}
    {source && (file ? <details className="patch-review-raw"><summary>Raw patch</summary><RawPatch source={source} label={label} /></details> : <RawPatch source={source} label={label} />)}
  </div>;
}

function RawPatch({ source, label }: { source: string; label: string }) {
  const [page, setPage] = useState(0);
  // Raw fallback is paged too: one pathological line cannot create a 1 MB DOM node.
  const bounded = source.slice(0, RAW_LIMIT);
  return <>
    <pre className="patch-review-source" tabIndex={0} aria-label={label}>{bounded.slice(page * SOURCE_PAGE, (page + 1) * SOURCE_PAGE)}</pre>
    {bounded.length > SOURCE_PAGE && <div className="patch-review-controls">
      <button type="button" disabled={page === 0} onClick={() => setPage(page - 1)}>Previous text</button>
      <span>Text page {page + 1} of {Math.ceil(bounded.length / SOURCE_PAGE)}</span>
      <button type="button" disabled={(page + 1) * SOURCE_PAGE >= bounded.length} onClick={() => setPage(page + 1)}>Next text</button>
    </div>}
    {source.length > RAW_LIMIT && <p className="patch-review-notice">Raw preview limited to 32,000 characters. Save the patch for its complete text.</p>}
  </>;
}
