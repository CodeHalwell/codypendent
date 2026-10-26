/**
 * Shared chrome for the knowledge surfaces.
 *
 * The important piece here is {@link SurfaceBody}: it is the single place that
 * decides between "could not read" and "read, and there is nothing". Every
 * surface routes through it so the distinction cannot be lost by accident in
 * one view's JSX (`InboxView.tsx` established the idiom; this generalises it).
 */
import React from "react";
import { EmptyState } from "./PageShell.js";
import type { LoadStatus } from "./knowledgeTransport.js";

export const surfaceStyles = {
  page: {
    flex: 1,
    display: "flex",
    flexDirection: "column",
    height: "100%",
    overflow: "hidden",
    background: "var(--cody-canvas)",
  } as React.CSSProperties,
  header: {
    padding: "26px 32px",
    borderBottom: "1px solid var(--cody-border)",
    display: "flex",
    alignItems: "center",
    flexWrap: "wrap",
    justifyContent: "space-between",
    gap: 12,
  } as React.CSSProperties,
  title: { margin: 0, fontSize: 25, fontWeight: 600, letterSpacing: -.7, lineHeight: 1.25, color: "var(--cody-text)" } as React.CSSProperties,
  subtitle: { fontSize: 13, color: "var(--cody-text-muted)", marginTop: 8, maxWidth: 660, lineHeight: 1.6 } as React.CSSProperties,
  scroll: { flex: 1, minHeight: 0, overflowY: "auto", padding: "24px 32px" } as React.CSSProperties,
  card: {
    border: "1px solid var(--cody-border)",
    borderRadius: 12,
    background: "var(--cody-panel-raised)",
    padding: 18,
    marginBottom: 14,
  } as React.CSSProperties,
  meta: { fontSize: 12, color: "var(--cody-text-muted)", marginTop: 4 } as React.CSSProperties,
  mono: {
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
    fontSize: 12,
  } as React.CSSProperties,
};

export const surfaceButton = (tone: "neutral" | "danger" | "primary" = "neutral"): React.CSSProperties => ({
  background: tone === "danger" ? "var(--cody-danger-bg)" : tone === "primary" ? "var(--cody-accent-strong)" : "var(--cody-inset)",
  color: tone === "danger" ? "var(--cody-danger-text)" : tone === "primary" ? "var(--cody-on-accent)" : "var(--cody-text)",
  border: `1px solid ${tone === "danger" ? "var(--cody-danger)" : "var(--cody-border-strong)"}`,
  borderRadius: 8,
  padding: "7px 12px",
  fontSize: 13,
  cursor: "pointer",
});

export interface SurfaceBodyProps {
  status: LoadStatus;
  /** Why the read failed. Shown verbatim when `status` is `"unavailable"`. */
  detail: string | null;
  /** How many rows the read returned. */
  count: number;
  /** The sentence shown when the read succeeded and returned nothing. */
  emptyMessage: string;
  emptyTitle?: string;
  emptyAction?: React.ReactNode;
  onRetry?: () => void;
  children: React.ReactNode;
}

/**
 * Renders `children` only once a read actually succeeded and returned rows.
 *
 * `"unavailable"` shows the reason and never the empty state; `"unloaded"` and
 * `"loading"` say so rather than implying an answer.
 */
export const SurfaceBody: React.FC<SurfaceBodyProps> = ({
  status,
  detail,
  count,
  emptyMessage,
  emptyTitle = "Nothing here yet",
  emptyAction,
  onRetry,
  children,
}) => {
  if (status === "unavailable") {
    return <EmptyState state="unavailable" title="Couldn’t load this page" description={detail ?? "No reason was reported."}
      action={onRetry && <button style={surfaceButton()} onClick={onRetry}>Try again</button>} />;
  }
  if (status === "unloaded") {
    return <EmptyState title="Not loaded yet" description="Load the latest information from your workspace."
      action={onRetry && <button style={surfaceButton()} onClick={onRetry}>Load now</button>} />;
  }
  if (status === "loading") {
    return <EmptyState state="loading" title="Loading…" description="Getting the latest workspace information." />;
  }
  if (count === 0) {
    return <EmptyState title={emptyTitle} description={emptyMessage} action={emptyAction} />;
  }
  return <>{children}</>;
};

/** A dim label / value pair used across the surface cards. */
export const Field: React.FC<{ label: string; value: React.ReactNode }> = ({ label, value }) => (
  <span style={{ fontSize: 12, color: "var(--cody-text-muted)", marginRight: 14 }}>
    {label} <span style={{ color: "var(--cody-text-secondary)" }}>{value}</span>
  </span>
);

/**
 * A host-owned confirmation.
 *
 * Trust transitions (enabling, approving, revoking a plugin; deleting a
 * learning; publishing a document) never fire straight off a click. The
 * evidence the operator is consenting to travels ON this prompt, so the
 * decisive moment never loses it.
 */
export const ConfirmPanel: React.FC<{
  title: string;
  /** The exact daemon-supplied evidence. Rendered verbatim, never summarised. */
  evidence?: React.ReactNode;
  confirmLabel: string;
  tone?: "neutral" | "danger" | "primary";
  onConfirm: () => void;
  onCancel: () => void;
}> = ({ title, evidence, confirmLabel, tone = "danger", onConfirm, onCancel }) => (
  <div
    role="dialog"
    aria-label={title}
    style={{
      border: "1px solid var(--cody-danger)",
      background: "var(--cody-danger-bg)",
      borderRadius: 8,
      padding: 14,
      marginBottom: 12,
    }}
  >
    <div style={{ color: "var(--cody-danger-text)", fontSize: 13, fontWeight: 600, marginBottom: 8 }}>{title}</div>
    {evidence !== undefined && (
      <pre
        style={{
          ...surfaceStyles.mono,
          whiteSpace: "pre-wrap",
          wordBreak: "break-word",
          color: "var(--cody-text-secondary)",
          background: "var(--cody-canvas)",
          border: "1px solid var(--cody-border-strong)",
          borderRadius: 6,
          padding: 10,
          margin: "0 0 10px",
        }}
      >
        {evidence}
      </pre>
    )}
    <div style={{ display: "flex", gap: 8 }}>
      <button style={surfaceButton(tone)} onClick={onConfirm}>
        {confirmLabel}
      </button>
      <button style={surfaceButton()} onClick={onCancel}>
        Cancel
      </button>
    </div>
  </div>
);
