import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { parsePatchReview, isReviewablePath, MAX_PATCH_FILES, MAX_PATCH_LINES, REVIEW_PAGE_LINES } from "../src/patchReview.js";
import { PatchReview } from "../src/components/PatchReview.js";
import { Transcript } from "../src/components/Transcript.js";

const patch = `diff --git a/src/main.ts b/src/main.ts
index 1234567..abcdef0 100644
--- a/src/main.ts
+++ b/src/main.ts
@@ -5,2 +5,3 @@ function main()
 context
-old
+new
+extra
@@ -20 +21 @@
-last
+final
`;
const addition = `diff --git a/new.ts b/new.ts
new file mode 100644
--- /dev/null
+++ b/new.ts
@@ -0,0 +1,2 @@
+first
+second
`;
const deletion = `diff --git a/gone.ts b/gone.ts
deleted file mode 100644
--- a/gone.ts
+++ /dev/null
@@ -1,2 +0,0 @@
-first
-last
`;

describe("patch review parsing", () => {
  it("keeps exact old/new coordinates through edits and multiple hunks", () => {
    const result = parsePatchReview(patch);
    expect(result.notice).toBeUndefined();
    expect(result.files[0]).toMatchObject({ path: "src/main.ts", status: "modified", additions: 3, deletions: 2 });
    expect(result.files[0]!.hunks[0]!.lines).toMatchObject([
      { kind: "context", oldLine: 5, newLine: 5, targetLine: 5 },
      { kind: "deletion", oldLine: 6, targetLine: 6 },
      { kind: "addition", newLine: 6, targetLine: 6 },
      { kind: "addition", newLine: 7, targetLine: 7 },
    ]);
    expect(result.files[0]!.hunks[1]!.lines[1]).toMatchObject({ newLine: 21, targetLine: 21 });
  });
  it("understands creation, deletion, rename-only and binary entries", () => {
    const result = parsePatchReview(addition + deletion + `diff --git a/old.ts b/renamed.ts
similarity index 100%
rename from old.ts
rename to renamed.ts
diff --git a/image.png b/image.png
index 1234..5678 100644
Binary files a/image.png and b/image.png differ
`);
    expect(result.notice).toBeUndefined();
    expect(result.files.map((file) => file.status)).toEqual(["added", "deleted", "renamed", "binary"]);
    expect(result.files[1]!.path).toBe("gone.ts");
    expect(result.files[1]!.hunks[0]!.lines[1]!.targetLine).toBe(2);
    expect(result.files[2]!.oldPath).toBe("old.ts");
  });
  it("decodes git-quoted Unicode names and names with spaces", () => {
    const result = parsePatchReview('diff --git "a/caf\\303\\251 file.ts" "b/caf\\303\\251 file.ts"\n--- "a/caf\\303\\251 file.ts"\n+++ "b/caf\\303\\251 file.ts"\n@@ -1 +1 @@\n-old\n+new\n');
    expect(result.notice).toBeUndefined();
    expect(result.files[0]!.path).toBe("café file.ts");
  });
  it("treats header-looking code inside hunks as code and retains newline notes", () => {
    const result = parsePatchReview('--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n--- old\n+++ new\n\\ No newline at end of file\n');
    expect(result.notice).toBeUndefined();
    expect(result.files[0]!.hunks[0]!.lines.map((line) => line.text)).toEqual(["-- old", "++ new", "\\ No newline at end of file"]);
  });
  it("jumps from an EOF deletion to the last surviving current-file line", () => {
    const result = parsePatchReview('--- a/a.ts\n+++ b/a.ts\n@@ -2,2 +2 @@\n kept\n-deleted\n');
    expect(result.files[0]!.hunks[0]!.lines[1]!.targetLine).toBe(2);
  });
  it.each(["../secret", "/tmp/secret", "a/../../secret", "a/%2e%2e/secret", "C:/secret", "a\\secret", "a/./secret", "a\u0000secret"])("never exposes an unsafe parsed jump: %s", (path) => {
    expect(isReviewablePath(path)).toBe(false);
    const result = parsePatchReview(`--- a/${path}\n+++ b/${path}\n@@ -1 +1 @@\n-old\n+new\n`);
    expect(result.files).toEqual([]);
    expect(result.notice).toBeTruthy();
  });
  it("keeps completed changes when a preview ends inside the following hunk", () => {
    const result = parsePatchReview(patch + "@@ -40,2 +41,2 @@\n-only one line");
    expect(result.files[0]!.hunks).toHaveLength(2);
    expect(result.notice).toContain("partway");
  });
  it("refuses impossible coordinates and malformed hunk counts", () => {
    for (const header of ["@@ -0 +1 @@", "@@ -9007199254740991,2 +1 @@", "@@ -1,0 +1 @@"]) {
      const result = parsePatchReview(`--- a/a.ts\n+++ b/a.ts\n${header}\n-old\n+new`);
      expect(result.files).toEqual([]);
      expect(result.notice).toBeTruthy();
    }
  });
  it("bounds parsed file and line counts", () => {
    expect(parsePatchReview(addition.repeat(MAX_PATCH_FILES + 1)).files).toHaveLength(MAX_PATCH_FILES);
    const result = parsePatchReview(`--- /dev/null\n+++ b/large.ts\n@@ -0,0 +1,${MAX_PATCH_LINES + 1} @@\n${"+x\n".repeat(MAX_PATCH_LINES + 1)}`);
    expect(result.notice).toContain("20,000");
    expect(result.files).toEqual([]);
  });
});

describe("patch review interaction", () => {
  it("keeps navigation focused across text and metadata-only files", () => {
    render(<PatchReview source={patch + `diff --git a/old.ts b/renamed.ts\nsimilarity index 100%\nrename from old.ts\nrename to renamed.ts\n` + addition} label="Patch preview" />);
    const next = screen.getByRole("button", { name: "Next change" });
    next.focus();
    fireEvent.click(next);
    fireEvent.click(next);
    expect(screen.getByText("File metadata changed; no text hunks.")).toBeTruthy();
    expect(document.activeElement).toBe(next);
    fireEvent.click(next);
    expect(screen.getByRole("region", { name: "Diff for new.ts" })).toBeTruthy();
    const previous = screen.getByRole("button", { name: "Previous change" });
    previous.focus();
    fireEvent.click(previous);
    fireEvent.click(previous);
    expect(screen.getByText("final")).toBeTruthy();
    expect(document.activeElement).toBe(previous);
  });
  it("navigates files and hunks, and opens exact current-file coordinates", async () => {
    const open = vi.fn().mockResolvedValue(undefined);
    render(<PatchReview source={patch + addition} label="Patch preview" onOpenFile={open} />);
    expect(screen.getByRole("region", { name: "Diff for src/main.ts" })).toBeTruthy();
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Open src/main.ts at line 7" })));
    expect(open).toHaveBeenLastCalledWith("src/main.ts", 7);
    fireEvent.click(screen.getByRole("button", { name: "Next change" }));
    expect(screen.getByText("final")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Next change" }));
    expect(screen.getByRole("region", { name: "Diff for new.ts" })).toBeTruthy();
    fireEvent.click(within(screen.getByRole("navigation", { name: "Changed files" })).getByRole("button", { name: /src\/main.ts/ }));
    expect(screen.getByText("extra")).toBeTruthy();
    fireEvent.change(screen.getByRole("combobox", { name: "Change in file" }), { target: { value: "1" } });
    expect(screen.getByText("final")).toBeTruthy();
  });
  it("renders only one bounded page of a large hunk", () => {
    const source = `--- /dev/null\n+++ b/large.ts\n@@ -0,0 +1,450 @@\n${Array.from({ length: 450 }, (_, i) => `+row${i + 1}`).join("\n")}\n`;
    render(<PatchReview source={source} label="Patch preview" />);
    expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(REVIEW_PAGE_LINES + 1);
    expect(screen.queryByText("row201")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Next lines" }));
    expect(screen.getByText("row201")).toBeTruthy();
    expect(screen.queryByText("row1")).toBeNull();
  });
  it("keeps unsafe touched files inert and reports guarded native open failures", async () => {
    const open = vi.fn().mockRejectedValue(new Error("File is outside the selected repository"));
    render(<PatchReview files={["safe.ts", "../secret"]} label="Patch preview" onOpenFile={open} />);
    expect(screen.getByRole("button", { name: /\.\.\/secret/ })).toHaveProperty("disabled", true);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: /safe.ts/ })));
    expect(open).toHaveBeenCalledWith("safe.ts", 1);
    expect(screen.getByRole("alert").textContent).toContain("outside the selected repository");
  });
  it("resets navigation on a different patch and discards stale open errors", async () => {
    let reject!: (error: Error) => void;
    const open = vi.fn(() => new Promise<void>((_, no) => { reject = no; }));
    const { rerender } = render(<PatchReview source={patch} label="Patch preview" onOpenFile={open} />);
    fireEvent.click(screen.getByRole("button", { name: "Next change" }));
    fireEvent.click(screen.getByRole("button", { name: "Open in editor" }));
    rerender(<PatchReview source={addition} label="Patch preview" onOpenFile={open} />);
    await act(async () => reject(new Error("Old selection")));
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.getByText("first")).toBeTruthy();
  });
  it("preserves keyboard focus during an editor open and ignores errors after choosing another file", async () => {
    let reject!: (error: Error) => void;
    const open = vi.fn(() => new Promise<void>((_, no) => { reject = no; }));
    render(<PatchReview source={patch + addition} label="Patch preview" onOpenFile={open} />);
    const button = screen.getByRole("button", { name: "Open in editor" });
    button.focus();
    fireEvent.click(button);
    expect(document.activeElement).toBe(screen.getByRole("region", { name: "Change review" }));
    await act(async () => reject(new Error("File unavailable")));
    expect(document.activeElement).toBe(button);
    fireEvent.click(button);
    fireEvent.click(within(screen.getByRole("navigation", { name: "Changed files" })).getByRole("button", { name: /new.ts/ }));
    await act(async () => reject(new Error("Previous file failure")));
    expect(screen.queryByRole("alert")).toBeNull();
  });
  it("wires touched files through a transcript patch card", async () => {
    const open = vi.fn().mockResolvedValue(undefined);
    render(<Transcript connectionStatus="connected" onOpenFile={open} items={[{ id: "p", type: "system", text: "Patch proposed", timestamp: "2026-10-04T18:00:00Z", patchFiles: ["src/main.ts"], patchRunId: "original-run", diffPreview: patch }]} />);
    expect(screen.queryByRole("region", { name: "Diff for src/main.ts" })).toBeNull();
    fireEvent.click(screen.getByText("Patch proposed"));
    const openButton = await screen.findByRole("button", { name: "Open in editor" });
    await act(async () => fireEvent.click(openButton));
    expect(open).toHaveBeenCalledWith("src/main.ts", 5, "original-run");
  });
});
