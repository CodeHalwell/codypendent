/** Unified-diff review only: parsing never writes files or applies a patch.
 * Hunk coordinate/count checks follow the editor client's patch-review.ts.
 */
export const MAX_PATCH_CHARACTERS = 1_000_000;
export const MAX_PATCH_FILES = 100;
export const MAX_PATCH_LINES = 20_000;
export const MAX_PATCH_HUNKS = 1_000;
export const REVIEW_PAGE_LINES = 200;
export const MAX_DISPLAY_LINE = 4_000;

export interface ReviewLine {
  kind: "context" | "addition" | "deletion" | "note";
  text: string;
  oldLine?: number;
  newLine?: number;
  /** Current-file coordinate; deleted rows use the nearest surviving line. */
  targetLine?: number;
}
export interface ReviewHunk { header: string; oldStart: number; newStart: number; lines: ReviewLine[] }
export interface ReviewFile {
  path: string;
  oldPath: string;
  newPath: string;
  status: "added" | "deleted" | "renamed" | "modified" | "binary";
  hunks: ReviewHunk[];
  additions: number;
  deletions: number;
  metadata: string[];
}
export interface PatchReviewResult { files: ReviewFile[]; notice?: string }

/** Native containment is authoritative; do not offer obvious unsafe jumps. */
export function isReviewablePath(path: string): boolean {
  return Boolean(path) && path !== "/dev/null" && !path.startsWith("/")
    && !/[\\%:\u0000-\u001f\u007f]/.test(path)
    && path.split("/").every((part) => part !== "" && part !== "." && part !== "..");
}

// Git quotes UTF-8 bytes using C/octal escapes when core.quotePath is enabled.
function unquotePath(value: string): string {
  if (!value.startsWith('"')) return value;
  if (!value.endsWith('"')) throw new Error("Incomplete quoted filename");
  const bytes: number[] = [];
  const encoder = new TextEncoder();
  for (let i = 1; i < value.length - 1;) {
    const ch = value[i]!;
    if (ch !== "\\") {
      const cp = String.fromCodePoint(value.codePointAt(i)!);
      bytes.push(...encoder.encode(cp)); i += cp.length; continue;
    }
    i++;
    const octal = /^[0-7]{1,3}/.exec(value.slice(i));
    if (octal) {
      const byte = parseInt(octal[0], 8);
      if (byte > 255) throw new Error("Invalid filename byte");
      bytes.push(byte); i += octal[0].length; continue;
    }
    const escapes: Record<string, number> = { '"': 34, "\\": 92, a: 7, b: 8, t: 9, n: 10, v: 11, f: 12, r: 13 };
    const escaped = escapes[value[i] ?? ""];
    if (escaped === undefined) throw new Error("Unsupported filename escape");
    bytes.push(escaped); i++;
  }
  return new TextDecoder("utf-8", { fatal: true }).decode(new Uint8Array(bytes));
}
function pathFromHeader(value: string, stripPrefix = true): string {
  let name: string;
  if (value.startsWith('"')) {
    const quoted = /^"(?:[^"\\]|\\.)*"/.exec(value)?.[0];
    if (!quoted || (value.slice(quoted.length) !== "" && !value.slice(quoted.length).startsWith("\t"))) {
      throw new Error("Malformed quoted filename");
    }
    name = unquotePath(quoted);
  } else name = value.split("\t", 1)[0]!;
  if (name === "/dev/null") return name;
  if (stripPrefix) name = name.replace(/^[ab]\//, "");
  if (!isReviewablePath(name)) throw new Error("Unsafe or unsupported patch filename");
  return name;
}
function gitPaths(line: string): [string, string] {
  const header = line.slice("diff --git ".length);
  const match = /^("(?:[^"\\]|\\.)*"|a\/.+) ("(?:[^"\\]|\\.)*"|b\/.+)$/.exec(header);
  if (!match) throw new Error("Malformed git file header");
  return [pathFromHeader(match[1]!), pathFromHeader(match[2]!)];
}
function createFile(oldPath: string, newPath: string): ReviewFile {
  if (oldPath === "/dev/null" && newPath === "/dev/null") throw new Error("Patch has no file");
  return { path: newPath === "/dev/null" ? oldPath : newPath, oldPath, newPath,
    status: oldPath === "/dev/null" ? "added" : newPath === "/dev/null" ? "deleted" : oldPath !== newPath ? "renamed" : "modified",
    hunks: [], additions: 0, deletions: 0, metadata: [] };
}

/** Keep complete, validated files when a bounded preview ends mid-patch. */
export function parsePatchReview(source: string): PatchReviewResult {
  const files: ReviewFile[] = [];
  const clipped = source.length > MAX_PATCH_CHARACTERS;
  const lines = source.slice(0, MAX_PATCH_CHARACTERS).split("\n");
  let file: ReviewFile | null = null;
  let countedLines = 0;
  let countedHunks = 0;
  let headerSeen = false;
  const finish = () => {
    if (file && (headerSeen || file.metadata.length || file.hunks.length)) files.push(file);
    file = null; headerSeen = false;
    if (files.length >= MAX_PATCH_FILES) throw new Error(`Showing the first ${MAX_PATCH_FILES} files`);
  };
  try {
    for (let i = 0; i < lines.length;) {
      const line = lines[i]!.replace(/\r$/, "");
      if (line.startsWith("diff --git ")) {
        finish();
        const [oldPath, newPath] = gitPaths(line);
        file = createFile(oldPath, newPath); i++; continue;
      }
      if (line.startsWith("--- ")) {
        if (!lines[i + 1]?.startsWith("+++ ")) throw new Error("Patch preview ends before the file header is complete");
        if (headerSeen || file?.hunks.length) finish();
        const parsed = createFile(pathFromHeader(line.slice(4)), pathFromHeader(lines[i + 1]!.slice(4).replace(/\r$/, "")));
        parsed.metadata = file?.metadata ?? [];
        file = parsed; headerSeen = true; i += 2; continue;
      }
      if (line.startsWith("@@ ")) {
        if (!file || !headerSeen) throw new Error("Patch hunk has no file header");
        const match = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(?:.*)$/.exec(line);
        if (!match) throw new Error("Unsupported patch hunk header");
        const [oldStart, oldCount, newStart, newCount] = [Number(match[1]), Number(match[2] ?? 1), Number(match[3]), Number(match[4] ?? 1)];
        if (![oldStart, oldCount, newStart, newCount].every(Number.isSafeInteger)
          || !Number.isSafeInteger(oldStart + oldCount) || !Number.isSafeInteger(newStart + newCount)
          || (oldStart === 0 && oldCount !== 0) || (newStart === 0 && newCount !== 0)) throw new Error("Invalid patch line coordinates");
        let oldLine = oldStart, newLine = newStart, oldRead = 0, newRead = 0;
        if (++countedHunks > MAX_PATCH_HUNKS) throw new Error(`Review limited to ${MAX_PATCH_HUNKS.toLocaleString()} changes`);
        const hunk: ReviewHunk = { header: line, oldStart, newStart, lines: [] };
        i++;
        while (oldRead < oldCount || newRead < newCount) {
          const row = lines[i];
          if (row === undefined) throw new Error("Patch preview ends partway through a change");
          const kind = row[0];
          if (kind === "\\" && row === "\\ No newline at end of file" && hunk.lines.length) {
            hunk.lines.push({ kind: "note", text: row }); i++; continue;
          }
          if (kind !== " " && kind !== "+" && kind !== "-") throw new Error("Patch preview is incomplete or has invalid hunk counts");
          const targetLine = file.newPath === "/dev/null" ? oldLine
            : kind === "-" ? Math.min(newLine, Math.max(1, newStart + newCount - 1)) : newLine;
          const entry: ReviewLine = { kind: kind === "+" ? "addition" : kind === "-" ? "deletion" : "context", text: row.slice(1), targetLine: Math.max(1, targetLine) };
          if (kind !== "+") { entry.oldLine = oldLine++; oldRead++; }
          if (kind !== "-") { entry.newLine = newLine++; newRead++; }
          if (oldRead > oldCount || newRead > newCount) throw new Error("Patch hunk counts do not match its lines");
          if (++countedLines > MAX_PATCH_LINES) throw new Error(`Review limited to ${MAX_PATCH_LINES.toLocaleString()} lines`);
          hunk.lines.push(entry); i++;
        }
        if (lines[i] === "\\ No newline at end of file") {
          hunk.lines.push({ kind: "note", text: lines[i]! }); i++;
        }
        file.hunks.push(hunk);
        file.additions += newCount - hunk.lines.filter((row) => row.kind === "context").length;
        file.deletions += oldCount - hunk.lines.filter((row) => row.kind === "context").length;
        continue;
      }
      if (file && /^(?:index |old mode |new mode |new file mode |deleted file mode |similarity index |dissimilarity index |rename from |rename to |copy from |copy to |Binary files |GIT binary patch)/.test(line)) {
        if (line.startsWith("rename from ") || line.startsWith("copy from ")) file.oldPath = pathFromHeader(line.slice(line.indexOf(" from ") + 6), false);
        if (line.startsWith("rename to ") || line.startsWith("copy to ")) { file.newPath = pathFromHeader(line.slice(line.indexOf(" to ") + 4), false); file.path = file.newPath; file.status = "renamed"; }
        if (line.startsWith("new file mode ")) { file.oldPath = "/dev/null"; file.status = "added"; }
        if (line.startsWith("deleted file mode ")) { file.newPath = "/dev/null"; file.status = "deleted"; }
        if (line.startsWith("Binary files ") || line === "GIT binary patch") file.status = "binary";
        file.metadata.push(line); i++;
        // Git binary payloads have no readable hunks; retain the file and skip.
        if (line === "GIT binary patch") while (i < lines.length && !lines[i]!.startsWith("diff --git ")) i++;
        continue;
      }
      if (line === "" || line === "\r") { i++; continue; }
      throw new Error("This patch format cannot be displayed as a line-by-line review");
    }
    finish();
    return { files, ...(clipped ? { notice: "Review limited to the first 1 MB of patch text" } : {}) };
  } catch (error) {
    // Preserve completed hunks from the final file, but never invent rows for
    // the partial hunk or expose a filename which failed validation.
    if (file?.hunks.length) files.push(file);
    return { files, notice: error instanceof Error ? error.message : "Could not parse this patch" };
  }
}
