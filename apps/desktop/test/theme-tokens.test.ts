/**
 * Colour lives in `theme.css`, nowhere else.
 *
 * The review counted 928 hard-coded hex literals across the desktop source:
 * a theme could not change them, and the light palette applied to the
 * chrome while every component kept its dark-only colours. The migration
 * replaced each with a role token; these guard the result so a new
 * component cannot quietly bring a literal back, and so no component can
 * reference a token the stylesheet does not define in every palette.
 */
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

const SRC = join(__dirname, "..", "src");

function sourceFiles(dir: string): string[] {
  const files: string[] = [];
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) {
      files.push(...sourceFiles(path));
    } else if (/\.tsx?$/.test(entry)) {
      files.push(path);
    }
  }
  return files;
}

const HEX_LITERAL = /#(?:[0-9a-f]{8}|[0-9a-f]{6}|[0-9a-f]{3})\b/gi;
const TOKEN_USE = /var\((--cody-[a-z-]+)/g;
const TOKEN_DEFINITION = /^\s*(--cody-[a-z-]+):/gm;

describe("theme tokens", () => {
  const files = sourceFiles(SRC);
  const theme = readFileSync(join(SRC, "theme.css"), "utf8");

  it("leaves no hex colour literal in the components", () => {
    const offenders: string[] = [];
    for (const file of files) {
      const text = readFileSync(file, "utf8");
      for (const match of text.matchAll(HEX_LITERAL)) {
        offenders.push(`${file.slice(SRC.length + 1)}: ${match[0]}`);
      }
    }
    expect(offenders).toEqual([]);
  });

  it("defines every token a component uses, in the dark palette and both light blocks", () => {
    // The stylesheet has three palette blocks: the dark `:root`, the light
    // media query, and the explicit `data-theme="light"` override. A token
    // that is missing from one of them falls through to the wrong palette.
    const blocks = theme.split(/(?=^:root|^@media)/m).filter((block) => /--cody-[\w-]+\s*:/.test(block));
    expect(blocks.length).toBe(3);
    const definedEverywhere = new Set<string>();
    const perBlock = blocks.map((block) => new Set(Array.from(block.matchAll(TOKEN_DEFINITION), (m) => m[1])));
    for (const token of perBlock[0]) {
      if (perBlock.every((set) => set.has(token))) {
        definedEverywhere.add(token);
      }
    }
    // Fonts are palette-independent and defined once.
    definedEverywhere.add("--cody-font");
    definedEverywhere.add("--cody-mono");

    const missing = new Set<string>();
    for (const file of files) {
      const text = readFileSync(file, "utf8");
      for (const match of text.matchAll(TOKEN_USE)) {
        if (!definedEverywhere.has(match[1])) {
          missing.add(`${match[1]} (${file.slice(SRC.length + 1)})`);
        }
      }
    }
    expect(Array.from(missing).sort()).toEqual([]);
  });
});
