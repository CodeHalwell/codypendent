/**
 * The command palette — the desktop's port of `Overlay::Palette`.
 *
 * The ranking is `crates/tui/src/palette.rs::palette_match_score`, ported
 * exactly, so `mode` beats `model` here the same way it does in the TUI. The
 * table is not hard-coded here. The app passes the rows it curates, and
 * `completePaletteEntries` fills the remainder from `NAV_GROUPS` — the same
 * table the sidebar renders — so the palette advertises exactly the views this
 * build mounts: never one that does not exist, and never one short
 * (`palette.rs`: "a front door to existing commands, never a second code
 * path").
 */
import React, { useEffect, useMemo, useRef, useState } from "react";
import { NAV_GROUPS, type DesktopView } from "./Navigation.js";
import { useFocusTrap } from "../useFocusTrap.js";

export interface PaletteEntry {
  /** Stable id the app dispatches on. */
  id: string;
  /** What the user reads and matches on. */
  title: string;
  description: string;
  /** The keyboard equivalent, or `"—"` when there is none. */
  key: string;
  /** `Setup` / `Run` / `Models` / `Workspace` / `Session`. */
  group: string;
}

/**
 * Lower is a stronger match. A verbatim port of `palette_match_score`
 * (`crates/tui/src/palette.rs`) — exact title 0, exact word 1, key 2, title
 * prefix 3, title substring 4, description prefix 5, description substring 6.
 */
export function paletteMatchScore(entry: PaletteEntry, needle: string): number | null {
  const title = entry.title.toLowerCase();
  const description = entry.description.toLowerCase();
  const key = entry.key.toLowerCase();

  if (title === needle) return 0;
  if (title.split(/[^\p{L}\p{N}]/u).some((word) => word === needle)) return 1;
  if (key === needle) return 2;
  if (title.startsWith(needle)) return 3;
  if (title.includes(needle)) return 4;
  if (description.startsWith(needle)) return 5;
  if (description.includes(needle)) return 6;
  return null;
}

/** The entries matching `query`, ranked by intent; table order breaks ties. */
export function filterPalette(
  entries: readonly PaletteEntry[],
  query: string,
): PaletteEntry[] {
  const needle = query.trim().toLowerCase();
  if (needle.length === 0) {
    return [...entries];
  }
  return entries
    .map((entry, tableIndex) => ({ entry, tableIndex, score: paletteMatchScore(entry, needle) }))
    .filter((row): row is { entry: PaletteEntry; tableIndex: number; score: number } =>
      row.score !== null,
    )
    .sort((left, right) => left.score - right.score || left.tableIndex - right.tableIndex)
    .map((row) => row.entry);
}

/** The palette command id that selects `view`. */
export function viewCommandId(view: DesktopView): string {
  return `view:${view}`;
}

/**
 * The entries the app supplied, plus a derived entry for every `DesktopView`
 * the app did not supply one for.
 *
 * The palette is the only surface that has to reach EVERY view: a surface you
 * can only find by hunting the sidebar is half-shipped, and a view added to
 * the sidebar but forgotten in a hand-written palette table is the same defect
 * one layer up. Deriving the remainder from `NAV_GROUPS` — the table the
 * sidebar itself renders — makes that omission impossible rather than merely
 * unlikely.
 *
 * The app's own entries win where they exist, so their richer titles (the
 * `/slash` aliases the TUI palette uses) and their ordering survive; only the
 * gaps are filled, and they are filled from the sidebar's own label and
 * description, never from invented content.
 */
export function completePaletteEntries(entries: readonly PaletteEntry[]): PaletteEntry[] {
  const supplied = new Set(entries.map((entry) => entry.id));
  const derived: PaletteEntry[] = [];
  for (const { group, views } of NAV_GROUPS) {
    for (const [view, label, description] of views) {
      const id = viewCommandId(view);
      if (supplied.has(id)) {
        continue;
      }
      derived.push({ id, title: label, description, key: "—", group });
    }
  }
  return [...entries, ...derived];
}

export interface CommandPaletteProps {
  restoreFocusFallback?: React.RefObject<HTMLElement | null>;
  open: boolean;
  entries: readonly PaletteEntry[];
  onRun: (id: string) => void;
  onClose: () => void;
}

export const CommandPalette: React.FC<CommandPaletteProps> = ({
  restoreFocusFallback,
  open,
  entries,
  onRun,
  onClose,
}) => {
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState(0);
  const inputRef = useRef<HTMLInputElement | null>(null);
  const dialogRef = useRef<HTMLDivElement | null>(null);
  // Focus stays inside the dialog and goes back to whatever opened it. Tab
  // used to walk straight out into the sidebar behind an `aria-modal` dialog.
  useFocusTrap(dialogRef, { active: open, initialFocus: inputRef, onEscape: onClose, restoreFocusFallback });

  // Never `entries` directly: the palette offers every mounted view, whether
  // or not the app remembered to list it.
  const rows = useMemo(() => completePaletteEntries(entries), [entries]);
  const matches = useMemo(() => filterPalette(rows, query), [rows, query]);

  useEffect(() => {
    if (open) {
      setQuery("");
      setSelected(0);
      inputRef.current?.focus();
    }
  }, [open]);

  // Keep the keyboard highlight on screen: arrow-keying past the visible
  // window used to move an OFF-SCREEN highlight in the scrollable list.
  const activeRowRef = useRef<HTMLButtonElement | null>(null);
  useEffect(() => {
    activeRowRef.current?.scrollIntoView({ block: "nearest" });
  }, [selected, open]);

  if (!open) {
    return null;
  }

  const handleKeyDown = (event: React.KeyboardEvent<HTMLInputElement>) => {
    if (event.nativeEvent.isComposing || event.nativeEvent.keyCode === 229) return;
    if ((event.key === "Home" || event.key === "End") && (event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      setSelected(event.key === "Home" ? 0 : Math.max(0, matches.length - 1));
      return;
    }
    if (event.key === "PageDown" || event.key === "PageUp") {
      event.preventDefault();
      setSelected((current) => Math.max(0, Math.min(matches.length - 1, current + (event.key === "PageDown" ? 7 : -7))));
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      onClose();
      return;
    }
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setSelected((current) => (matches.length === 0 ? 0 : (current + 1) % matches.length));
      return;
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      setSelected((current) =>
        matches.length === 0 ? 0 : (current - 1 + matches.length) % matches.length,
      );
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      // A zero-match filter runs NOTHING. It must not fall back to row 0 —
      // the TUI pickers carry the same guard, and it was a real bug there.
      const entry = matches[selected];
      if (entry) {
        onRun(entry.id);
      }
    }
  };

  // Group labels are rendered only while the query is empty: filtering can
  // straddle groups, so the labels would mislead (`palette.rs`).
  const showGroups = query.trim().length === 0;
  let lastGroup: string | null = null;

  return (
    <div
      style={{
        position: "fixed",
        inset: 0,
        background: "rgba(1, 4, 9, 0.6)",
        display: "flex",
        alignItems: "flex-start",
        justifyContent: "center",
        paddingTop: "12vh",
        zIndex: 1000,
      }}
      onClick={onClose}
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-label="Command palette"
        onClick={(event) => event.stopPropagation()}
        style={{
          width: "min(680px, 90vw)",
          maxHeight: "70vh",
          display: "flex",
          flexDirection: "column",
          background: "var(--cody-panel-raised)",
          border: "1px solid var(--cody-border-strong)",
          borderRadius: 10,
          overflow: "hidden",
          boxShadow: "0 16px 48px rgba(1, 4, 9, 0.7)",
        }}
      >
        <input
          ref={inputRef}
          value={query}
          role="combobox"
          aria-expanded="true"
          aria-controls="command-palette-listbox"
          aria-activedescendant={matches[selected] ? `palette-option-${matches[selected].id}` : undefined}
          aria-label="Command palette filter"
          placeholder="Run a command…"
          onChange={(event) => {
            setQuery(event.target.value);
            setSelected(0);
          }}
          onKeyDown={handleKeyDown}
          style={{
            background: "var(--cody-canvas)",
            border: "none",
            borderBottom: "1px solid var(--cody-border-strong)",
            outline: "none",
            color: "var(--cody-text)",
            fontSize: 14,
            padding: "12px 16px",
          }}
        />
        <div role="listbox" id="command-palette-listbox" style={{ overflowY: "auto" }}>
          {matches.length === 0 ? (
            <div style={{ padding: 16, color: "var(--cody-text-faint)", fontSize: 13 }}>
              No command matches “{query.trim()}”.
            </div>
          ) : (
            matches.map((entry, index) => {
              const groupLabel = showGroups && entry.group !== lastGroup ? entry.group : null;
              lastGroup = entry.group;
              const active = index === selected;
              return (
                <React.Fragment key={entry.id}>
                  {groupLabel && (
                    <div
                      style={{
                        padding: "10px 16px 4px",
                        fontSize: 10,
                        letterSpacing: 0.6,
                        textTransform: "uppercase",
                        color: "var(--cody-text-faint)",
                      }}
                    >
                      {groupLabel}
                    </div>
                  )}
                  <button
                    ref={active ? activeRowRef : undefined}
                    id={`palette-option-${entry.id}`}
                    role="option"
                    aria-selected={active}
                    onMouseEnter={() => setSelected(index)}
                    onClick={() => onRun(entry.id)}
                    aria-label={entry.title}
                    data-command-id={entry.id}
                    style={{
                      display: "flex",
                      width: "100%",
                      alignItems: "baseline",
                      justifyContent: "space-between",
                      gap: 12,
                      textAlign: "left",
                      padding: "8px 16px",
                      border: "none",
                      borderLeft: `2px solid ${active ? "var(--cody-accent)" : "transparent"}`,
                      background: active ? "var(--cody-panel-hover)" : "transparent",
                      color: "var(--cody-text)",
                      cursor: "pointer",
                    }}
                  >
                    <span style={{ minWidth: 0 }}>
                      <span style={{ fontSize: 13, fontWeight: active ? 600 : 400 }}>
                        {entry.title}
                      </span>
                      <span
                        style={{
                          display: "block",
                          fontSize: 11,
                          color: "var(--cody-text-muted)",
                          marginTop: 2,
                        }}
                      >
                        {entry.description}
                      </span>
                    </span>
                    <span style={{ fontSize: 11, color: "var(--cody-text-faint)", whiteSpace: "nowrap" }}>
                      {entry.key}
                    </span>
                  </button>
                </React.Fragment>
              );
            })
          )}
        </div>
      </div>
    </div>
  );
};
