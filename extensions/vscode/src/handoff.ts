/** Untrusted editor URI boundary. No credentials or commands travel in links. */
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import { validateSocketPath } from "./protocol/discovery.js";

export interface SessionHandoff {
  session: string;
  repository: string;
  checkout?: string;
  socket: string;
  file?: string;
  line?: number;
}

function hasPathControl(value: string): boolean {
  for (let index = 0; index < value.length; index++) {
    const code = value.charCodeAt(index);
    if (code <= 0x1f || code === 0x7f) return true;
  }
  return false;
}

export function parseHandoff(query: string): SessionHandoff {
  if (query.length > 16_384) throw new Error("Session link is too long");
  const params = new URLSearchParams(query);
  const allowed = new Set(["session", "repository", "checkout", "socket", "file", "line"]);
  for (const key of params.keys()) {
    if (!allowed.has(key) || params.getAll(key).length !== 1) {
      throw new Error("Session link contains an unknown or repeated field");
    }
  }
  const session = params.get("session") ?? "";
  const repository = params.get("repository") ?? "";
  const socket = params.get("socket") ?? "";
  const checkout = params.get("checkout") ?? undefined;
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(session)) {
    throw new Error("Session link has an invalid session ID");
  }
  for (const value of [repository, socket, ...(checkout === undefined ? [] : [checkout])]) {
    if (!path.isAbsolute(value) || hasPathControl(value)) {
      throw new Error("Session link must name absolute local paths");
    }
  }
  validateSocketPath(socket);
  const file = params.get("file") ?? undefined;
  if (file !== undefined && (!file || file.includes("\\") || hasPathControl(file) || path.isAbsolute(file)
      || file.split("/").some((part) => !part || part === "." || part === ".."))) {
    throw new Error("Source location must stay inside the repository");
  }
  const lineText = params.get("line");
  const line = lineText === null ? undefined : Number(lineText);
  if (line !== undefined && (!file || !/^[1-9]\d*$/.test(lineText!)
      || !Number.isSafeInteger(line) || line > 2_147_483_647)) {
    throw new Error("Source location has an invalid line number");
  }
  return { session, repository, socket, ...(checkout === undefined ? {} : { checkout }), ...(file === undefined ? {} : { file }),
    ...(line === undefined ? {} : { line }) };
}

export function isInsideRepository(root: string, file: string): boolean {
  const relative = path.relative(root, file);
  return relative !== "" && relative !== ".." && !relative.startsWith(`..${path.sep}`)
    && !path.isAbsolute(relative);
}

/** Resolve symlinks again at use, and only connect to this user's Unix socket. */
export async function validateHandoff(handoff: SessionHandoff): Promise<SessionHandoff> {
  const repository = await fs.realpath(handoff.repository);
  if (!(await fs.stat(repository)).isDirectory() || repository === await fs.realpath(os.homedir())) {
    throw new Error("Choose a Git checkout, not your home directory");
  }
  await fs.stat(path.join(repository, ".git"));
  const checkout = handoff.checkout ? await fs.realpath(handoff.checkout) : repository;
  if (checkout !== repository && await commonGitDirectory(checkout) !== await commonGitDirectory(repository)) {
    throw new Error("The run's checkout does not belong to this repository");
  }
  const socket = await fs.realpath(handoff.socket);
  const socketStat = await fs.stat(socket);
  if (!socketStat.isSocket() || (process.getuid && socketStat.uid !== process.getuid())) {
    throw new Error("Session link must point to your own local daemon socket");
  }
  if (handoff.file) {
    const file = await fs.realpath(path.join(checkout, handoff.file));
    if (!isInsideRepository(checkout, file) || !(await fs.stat(file)).isFile()) {
      throw new Error("Source location is outside the repository or is not a file");
    }
  }
  return { ...handoff, repository, checkout, socket };
}

async function commonGitDirectory(checkout: string): Promise<string> {
  // Git's worktree metadata is enough to prove the common repository. Reading
  // it avoids launching an editor-inherited Git command on every file jump.
  const marker = path.join(checkout, ".git");
  let directory = marker;
  if (!(await fs.stat(marker)).isDirectory()) {
    const match = /^gitdir: ([^\r\n\0]+)\r?\n?$/.exec(await readGitPath(marker));
    if (!match) throw new Error("Invalid Git checkout metadata");
    directory = path.resolve(checkout, match[1]);
  }
  directory = await fs.realpath(directory);
  try {
    const common = (await readGitPath(path.join(directory, "commondir"))).trim();
    if (!common || /[\r\n\0]/.test(common)) throw new Error("Invalid Git common directory");
    directory = await fs.realpath(path.resolve(directory, common));
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
  }
  if (!(await fs.stat(directory)).isDirectory()) throw new Error("Git common directory is missing");
  return directory;
}

async function readGitPath(file: string): Promise<string> {
  const handle = await fs.open(file, "r");
  try {
    const buffer = Buffer.alloc(8_193);
    const { bytesRead } = await handle.read(buffer, 0, buffer.length, 0);
    if (bytesRead > 8_192) throw new Error("Git path metadata is too large");
    return buffer.subarray(0, bytesRead).toString("utf8");
  } finally { await handle.close(); }
}

export async function workspaceMatches(repository: string, roots: readonly string[]): Promise<boolean> {
  for (const root of roots) {
    try { if (await fs.realpath(root) === repository) return true; } catch { /* stale folder */ }
  }
  return false;
}
