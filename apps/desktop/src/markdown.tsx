import React from "react";
import { MarkdownLink, type MarkdownOptions } from "./markdownLinks.js";
export type { MarkdownOptions } from "./markdownLinks.js";

/**
 * The little of Markdown that model output actually uses, rendered to React
 * elements.
 *
 * **No HTML passthrough, by construction.** Every node here is a React element
 * built from parsed text; nothing is ever handed to `dangerouslySetInnerHTML`.
 * The content is model output arriving over a socket, so a renderer that could
 * emit arbitrary HTML into a webview would be an injection vector — which is
 * also why this is a hundred lines rather than a dependency.
 *
 * Supported, because it is what the models emit: ATX headings, fenced code
 * blocks (with a language strip and a Copy button), pipe tables, unordered
 * and ordered lists, blockquotes, horizontal rules, and inline `code`,
 * `**bold**`, `*italic*` and `[text](href)`. Source references use the
 * validated editor callback; web links retain a visible, copyable destination.
 *
 * Anything unrecognised is left exactly as written, which is the important
 * property: unsupported syntax degrades to the plain text it already was
 * rather than disappearing.
 */

const CODE_FONT = "ui-monospace, SFMono-Regular, Menlo, monospace";

const H_STYLES: Record<number, React.CSSProperties> = {
  1: { fontSize: 18, fontWeight: 700, margin: "12px 0 6px" },
  2: { fontSize: 16, fontWeight: 700, margin: "12px 0 6px" },
  3: { fontSize: 15, fontWeight: 600, margin: "10px 0 4px" },
  4: { fontSize: 14, fontWeight: 600, margin: "10px 0 4px" },
  5: { fontSize: 13, fontWeight: 600, margin: "8px 0 4px" },
  6: { fontSize: 13, fontWeight: 600, margin: "8px 0 4px", color: "var(--cody-text-muted)" },
};

const CODE_BLOCK: React.CSSProperties = {
  margin: 0,
  padding: "10px 12px",
  background: "var(--cody-canvas)",
  fontFamily: CODE_FONT,
  fontSize: 12,
  overflowX: "auto",
  whiteSpace: "pre",
};

const CODE_FRAME: React.CSSProperties = {
  margin: "8px 0",
  border: "1px solid var(--cody-border-strong)",
  borderRadius: 6,
  overflow: "hidden",
};

const CODE_STRIP: React.CSSProperties = {
  display: "flex",
  justifyContent: "space-between",
  alignItems: "center",
  padding: "3px 10px",
  background: "var(--cody-panel-raised)",
  borderBottom: "1px solid var(--cody-border-strong)",
  fontSize: 11,
  color: "var(--cody-text-muted)",
};

const TABLE: React.CSSProperties = {
  margin: "8px 0",
  borderCollapse: "collapse",
  fontSize: 13,
};

const TABLE_CELL: React.CSSProperties = {
  border: "1px solid var(--cody-border-strong)",
  padding: "4px 10px",
  textAlign: "left",
};

/**
 * A fenced block with a header strip: the language the fence declared (it
 * used to be parsed and thrown away), and a Copy button — getting a snippet
 * out used to mean a manual selection drag.
 */
const CodeBlock: React.FC<{ language: string; body: string }> = ({ language, body }) => {
  const [copied, setCopied] = React.useState(false);
  return (
    <div style={CODE_FRAME}>
      <div style={CODE_STRIP}>
        <span>{language || "code"}</span>
        <button
          type="button"
          onClick={() => {
            void navigator.clipboard
              ?.writeText(body)
              .then(() => {
                setCopied(true);
                setTimeout(() => setCopied(false), 1600);
              })
              .catch(() => {
                // writeText rejects when the document is unfocused or the
                // permission is denied; the label staying "Copy" is the signal.
              });
          }}
          style={{
            background: "transparent",
            border: "none",
            color: copied ? "var(--cody-success)" : "var(--cody-text-muted)",
            cursor: "pointer",
            fontSize: 11,
            padding: 0,
          }}
        >
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      <pre style={CODE_BLOCK}>{body}</pre>
    </div>
  );
};

/** A pipe-table row's cells, without the outer pipes. */
function tableCells(line: string): string[] {
  return line
    .trim()
    .replace(/^\|/, "")
    .replace(/\|$/, "")
    .split("|")
    .map((cell) => cell.trim());
}

/** Whether `line` is a table separator row: `|---|:--:|` and friends. */
function isTableSeparator(line: string): boolean {
  const trimmed = line.trim();
  if (!trimmed.startsWith("|")) {
    return false;
  }
  return tableCells(trimmed).every((cell) => /^:?-{3,}:?$/.test(cell));
}

const INLINE_CODE: React.CSSProperties = {
  fontFamily: CODE_FONT,
  fontSize: "0.92em",
  background: "var(--cody-canvas)",
  border: "1px solid var(--cody-border-strong)",
  borderRadius: 4,
  padding: "1px 4px",
};

const QUOTE: React.CSSProperties = {
  margin: "8px 0",
  padding: "2px 0 2px 12px",
  borderLeft: "3px solid var(--cody-border-strong)",
  color: "var(--cody-text-muted)",
};

const LIST: React.CSSProperties = { margin: "6px 0", paddingLeft: 22 };
const PARAGRAPH: React.CSSProperties = { margin: "6px 0", whiteSpace: "pre-wrap" };
const RULE: React.CSSProperties = { border: 0, borderTop: "1px solid var(--cody-border-strong)", margin: "12px 0" };

/** `**bold**`, `*italic*`, `` `code` `` and `[text](href)`, in one pass. */
const INLINE = /(`[^`]+`)|(\*\*[^*]+\*\*)|(\*[^*\n]+\*)|(\[[^\]\n]+\]\((?:<[^>\n]+>|[^)\n]+)\))/g;

function inline(text: string, key: string, options: MarkdownOptions): React.ReactNode[] {
  const out: React.ReactNode[] = [];
  let last = 0;
  let index = 0;
  for (const match of text.matchAll(INLINE)) {
    const at = match.index;
    if (at > last) {
      out.push(text.slice(last, at));
    }
    const token = match[0];
    const id = `${key}-i${index++}`;
    if (token.startsWith("`")) {
      out.push(
        <code key={id} style={INLINE_CODE}>
          {token.slice(1, -1)}
        </code>,
      );
    } else if (token.startsWith("**")) {
      // Recursed, so `**`code`**` renders as bold code rather than bold
      // backticks. The slice is strictly shorter, so this terminates.
      out.push(<strong key={id}>{inline(token.slice(2, -2), id, options)}</strong>);
    } else if (token.startsWith("[")) {
      const split = token.indexOf("](");
      const destination = token.slice(split + 2, -1);
      out.push(<MarkdownLink key={`${id}:${destination}`} label={token.slice(1, split)} destination={destination} {...options} />);
    } else {
      out.push(<em key={id}>{inline(token.slice(1, -1), id, options)}</em>);
    }
    last = at + token.length;
  }
  if (last < text.length) {
    out.push(text.slice(last));
  }
  return out;
}

/** Render `text` as Markdown. Returns React elements; never HTML. */
export function renderMarkdown(text: string, options: MarkdownOptions = {}): React.ReactNode {
  const lines = text.split("\n");
  const blocks: React.ReactNode[] = [];
  let paragraph: string[] = [];
  let key = 0;

  const flush = () => {
    if (paragraph.length > 0) {
      const body = paragraph.join("\n");
      blocks.push(
        <div key={`p${key++}`} style={PARAGRAPH}>
          {inline(body, `p${key}`, options)}
        </div>,
      );
      paragraph = [];
    }
  };

  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];

    // The info string may carry attributes after the language (```ts
    // title="x"); the old `\w*$` regex refused those fences entirely, so
    // their contents rendered as literal text.
    const fence = /^\s*```(\S*)(?:\s.*)?$/.exec(line);
    if (fence) {
      flush();
      const body: string[] = [];
      i += 1;
      while (i < lines.length && !/^\s*```\s*$/.test(lines[i])) {
        body.push(lines[i]);
        i += 1;
      }
      blocks.push(<CodeBlock key={`c${key++}`} language={fence[1]} body={body.join("\n")} />);
      continue;
    }

    // A pipe table: a header row, a separator row, then body rows. Anything
    // that only looks like one falls through and stays the text it was.
    if (
      line.trim().startsWith("|") &&
      i + 1 < lines.length &&
      isTableSeparator(lines[i + 1])
    ) {
      flush();
      const header = tableCells(line);
      const rows: string[][] = [];
      i += 2;
      while (i < lines.length && lines[i].trim().startsWith("|")) {
        rows.push(tableCells(lines[i]));
        i += 1;
      }
      i -= 1;
      blocks.push(
        <table key={`t${key++}`} style={TABLE}>
          <thead>
            <tr>
              {header.map((cell, at) => (
                <th key={`th${at}`} style={{ ...TABLE_CELL, fontWeight: 600 }}>
                  {inline(cell, `t${key}-h${at}`, options)}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.map((cells, rowAt) => (
              <tr key={`tr${rowAt}`}>
                {cells.map((cell, at) => (
                  <td key={`td${at}`} style={TABLE_CELL}>
                    {inline(cell, `t${key}-${rowAt}-${at}`, options)}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>,
      );
      continue;
    }

    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      flush();
      const level = heading[1].length;
      blocks.push(
        <div key={`h${key++}`} style={H_STYLES[level]}>
          {inline(heading[2], `h${key}`, options)}
        </div>,
      );
      continue;
    }

    if (/^\s*(---+|\*\*\*+|___+)\s*$/.test(line)) {
      flush();
      blocks.push(<hr key={`r${key++}`} style={RULE} />);
      continue;
    }

    if (/^\s*>\s?/.test(line)) {
      flush();
      const body: string[] = [];
      while (i < lines.length && /^\s*>\s?/.test(lines[i])) {
        body.push(lines[i].replace(/^\s*>\s?/, ""));
        i += 1;
      }
      i -= 1;
      blocks.push(
        <div key={`q${key++}`} style={QUOTE}>
          {inline(body.join("\n"), `q${key}`, options)}
        </div>,
      );
      continue;
    }

    if (/^\s*([-*+]|\d+\.)\s+/.test(line)) {
      flush();
      const ordered = /^\s*\d+\./.test(line);
      const items: string[] = [];
      while (i < lines.length && /^\s*([-*+]|\d+\.)\s+/.test(lines[i])) {
        items.push(lines[i].replace(/^\s*([-*+]|\d+\.)\s+/, ""));
        i += 1;
      }
      i -= 1;
      const children = items.map((item, at) => (
        <li key={`li${at}`}>{inline(item, `l${key}-${at}`, options)}</li>
      ));
      blocks.push(
        ordered ? (
          <ol key={`o${key++}`} style={LIST}>
            {children}
          </ol>
        ) : (
          <ul key={`u${key++}`} style={LIST}>
            {children}
          </ul>
        ),
      );
      continue;
    }

    if (line.trim() === "") {
      flush();
      continue;
    }
    paragraph.push(line);
  }
  flush();
  return <>{blocks}</>;
}
