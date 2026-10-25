/**
 * The strip above every secondary view: where you are, and the way back.
 *
 * The app has one working surface (Sessions) and twenty-one destinations
 * reached from the sidebar or the palette. Escape walks back through them —
 * `App.tsx` keeps a history — but nothing on screen said so, and the views
 * built without the shared header (Analytics, Code Graph, Council results)
 * had no back affordance at all: a dead end you left by aiming at the
 * sidebar again. This is the one place that affordance lives, for every
 * view, with the view's own name beside it.
 */
import React from "react";
import { ArrowLeft } from "lucide-react";

import { NAV_GROUPS, type DesktopView } from "./Navigation.js";

export interface ViewBarProps {
  view: DesktopView;
  onBack: () => void;
}

/** The sidebar label for a view, from the one table the sidebar renders. */
export function viewLabel(view: DesktopView): string {
  for (const { views } of NAV_GROUPS) {
    for (const [candidate, label] of views) {
      if (candidate === view) {
        return label;
      }
    }
  }
  return view;
}

const BAR: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 10,
  padding: "10px 24px",
  borderBottom: "1px solid var(--cody-border)",
  background: "var(--cody-canvas)",
  fontSize: 12,
  color: "var(--cody-text-muted)",
  flexShrink: 0,
};

const BACK: React.CSSProperties = {
  background: "transparent",
  border: "1px solid var(--cody-border-strong)",
  borderRadius: 6,
  color: "var(--cody-text)",
  padding: "3px 10px",
  fontSize: 12,
  cursor: "pointer",
  font: "inherit",
};

export const ViewBar: React.FC<ViewBarProps> = ({ view, onBack }) => (
  <nav aria-label="View navigation" data-testid="view-bar" style={BAR}>
    <button type="button" onClick={onBack} title="Back to the previous view (Esc)" style={BACK}>
      <ArrowLeft size={14} aria-hidden="true" style={{ verticalAlign: "middle", marginRight: 6 }} />Back
    </button>
    <span aria-current="page" style={{ color: "var(--cody-text-secondary)" }}>
      {viewLabel(view)}
    </span>
    <span style={{ marginLeft: "auto", fontSize: 11 }} title="Press Escape to go back">Esc to go back</span>
  </nav>
);
