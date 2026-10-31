import { afterEach, describe, expect, it } from "vitest";
import { mkdtemp, mkdir, writeFile, symlink, rm } from "node:fs/promises";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { parseHandoff, validateHandoff, workspaceMatches } from "../src/handoff.js";

const session = "11111111-2222-3333-4444-555555555555";
function query(extra: Record<string, string> = {}) {
  return new URLSearchParams({ session, repository: "/tmp/project with spaces", socket: "/tmp/cody.sock", ...extra }).toString();
}
const cleanup: Array<() => Promise<unknown>> = [];
afterEach(async () => { for (const close of cleanup.splice(0).reverse()) await close(); });

describe("editor session handoff", () => {
  it("decodes an exact session, repository, socket and one-based file location", () => {
    expect(parseHandoff(query({ file: "src/café test.ts", line: "42" }))).toEqual({
      session, repository: "/tmp/project with spaces", socket: "/tmp/cody.sock", file: "src/café test.ts", line: 42,
    });
  });
  it("rejects every C0 control and DEL in each path field without rejecting adjacent characters", () => {
    const codes = [...Array.from({ length: 32 }, (_, index) => index), 0x7f];
    for (const code of codes) {
      for (const field of ["repository", "checkout", "socket", "file"]) {
        const prefix = field === "file" ? "src/" : "/tmp/";
        expect(() => parseHandoff(query({ [field]: `${prefix}before${String.fromCharCode(code)}after` })))
          .toThrow();
      }
    }
    for (const code of [0x20, 0x7e, 0x80]) {
      const file = `src/before${String.fromCharCode(code)}after.ts`;
      expect(parseHandoff(query({ file })).file).toBe(file);
    }
  });
  it.each(["../secret", "src/../../secret", "/etc/passwd", "src\\outside", "src/./file", "src//file", "bad\nfile"])(
    "rejects an unsafe source path %j", (file) => expect(() => parseHandoff(query({ file }))).toThrow(),
  );
  it.each(["0", "-1", "1.2", "NaN", "4294967295", "1e2"])(
    "rejects an invalid line %s", (line) => expect(() => parseHandoff(query({ file: "file.ts", line }))).toThrow(),
  );
  it("rejects credentials, duplicate fields, nonlocal paths and malformed sessions", () => {
    expect(() => parseHandoff(query({ token: "do-not-transport-secrets" }))).toThrow();
    expect(() => parseHandoff(`${query()}&session=${session}`)).toThrow();
    expect(() => parseHandoff(query({ socket: "https://example.com" }))).toThrow();
    expect(() => parseHandoff(query({ session: "not-a-session" }))).toThrow();
    expect(() => parseHandoff(query({ line: "1" }))).toThrow();
  });
  it("validates a real owned socket and checkout, refusing symlink escapes and regular-file sockets", async () => {
    const dir = await mkdtemp(path.join(os.tmpdir(), "cody-hand-"));
    cleanup.push(() => rm(dir, { recursive: true, force: true }));
    const repository = path.join(dir, "repo");
    await mkdir(path.join(repository, ".git"), { recursive: true });
    await writeFile(path.join(repository, "file.ts"), "line one\nline two\n");
    const socket = path.join(dir, "d.sock");
    const server = net.createServer((connection) => connection.end());
    await new Promise<void>((resolve) => server.listen(socket, resolve));
    cleanup.push(() => new Promise<void>((resolve) => server.close(() => resolve())));
    const handoff = parseHandoff(query({ repository, socket, file: "file.ts", line: "2" }));
    const validated = await validateHandoff(handoff);
    expect(await workspaceMatches(validated.repository, [repository])).toBe(true);
    await writeFile(path.join(dir, "outside"), "secret");
    await symlink(path.join(dir, "outside"), path.join(repository, "escape"));
    await expect(validateHandoff({ ...handoff, file: "escape" })).rejects.toThrow("outside");
    await expect(validateHandoff({ ...handoff, socket: path.join(repository, "file.ts") })).rejects.toThrow("socket");
    const checkout = path.join(dir, "run-checkout");
    await mkdir(checkout);
    await mkdir(path.join(repository, ".git/worktrees/run"), { recursive: true });
    await writeFile(path.join(checkout, ".git"), `gitdir: ${repository}/.git/worktrees/run\n`);
    await writeFile(path.join(repository, ".git/worktrees/run/commondir"), "../..\n");
    await writeFile(path.join(checkout, "changed.ts"), "run change\n");
    const runHandoff = await validateHandoff({ ...handoff, checkout, file: "changed.ts" });
    expect(runHandoff.repository).toBe(validated.repository);
    expect(runHandoff.checkout).not.toBe(validated.repository);
    expect(await workspaceMatches(runHandoff.checkout!, [checkout])).toBe(true);
    const unrelated = path.join(dir, "unrelated");
    await mkdir(path.join(unrelated, ".git"), { recursive: true });
    await expect(validateHandoff({ ...handoff, checkout: unrelated })).rejects.toThrow("does not belong");
  }, 15_000);
});
