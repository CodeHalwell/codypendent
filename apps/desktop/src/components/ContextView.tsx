import { EmptyState, PageShell } from "./PageShell.js";
/**
 * `/context` — the context-usage breakdown for the active run.
 *
 * A PURE CLIENT PROJECTION over the event stream: the only source is
 * `EventBody::ContextUsage`, which the shell already delivers verbatim inside
 * `DaemonFrame::Event`. No bridge command is needed and none is invented.
 *
 * Two rules from `crates/tui/src/reduce.rs` and `render.rs` are load-bearing:
 *
 * 1. The report is STRICTLY scoped to its `run_id`. A usage report for a run
 *    this client has not materialised is never painted onto whichever run
 *    happens to be selected.
 * 2. Absent measurements stay absent. With no report the card prints the
 *    percentage if it is known, else `—`, then says the breakdown is not yet
 *    available — never zeros, never a synthesised split.
 */
import React from "react";
import type { SessionEvent } from "@codypendent/protocol";
import { surfaceStyles } from "./surfaceChrome.js";

export interface ContextBreakdown {
  used_tokens: number;
  window_tokens: number;
  system_tokens: number;
  tool_tokens: number;
  transcript_tokens: number;
}

/**
 * The latest `ContextUsage` for exactly `runId`, or `null`.
 *
 * Returns `null` — not zeros — when the provider has never reported, and
 * ignores every report addressed to a different run.
 */
export function latestContextUsage(
  events: readonly SessionEvent[],
  runId: string | null,
): ContextBreakdown | null {
  if (runId === null) {
    return null;
  }
  let latest: ContextBreakdown | null = null;
  for (const event of events) {
    const body = event.body;
    if (body.type === "ContextUsage" && body.run_id === runId) {
      latest = {
        used_tokens: body.used_tokens,
        window_tokens: body.window_tokens,
        system_tokens: body.system_tokens,
        tool_tokens: body.tool_tokens,
        transcript_tokens: body.transcript_tokens,
      };
    }
  }
  return latest;
}

/** `used * 100 / window`, capped at 100; `null` when the window is unknown. */
export function contextPercent(breakdown: ContextBreakdown | null): number | null {
  if (breakdown === null || breakdown.window_tokens === 0) {
    return null;
  }
  return Math.min(100, Math.floor((breakdown.used_tokens * 100) / breakdown.window_tokens));
}

function thousands(value: number): string {
  return value.toLocaleString("en-US");
}

export interface ContextViewProps {
  /** Every durable event this client holds for the attached session. */
  events: readonly SessionEvent[];
  activeRunId: string | null;
}

const Row: React.FC<{ label: string; tokens: number; color: string }> = ({
  label,
  tokens,
  color,
}) => (
  <div style={{ display: "flex", justifyContent: "space-between", maxWidth: 380, marginTop: 4 }}>
    <span style={{ fontSize: 12, color: "var(--cody-text-muted)" }}>{label}</span>
    <span style={{ ...surfaceStyles.mono, color }}>{thousands(tokens)} tokens</span>
  </div>
);

export const ContextView: React.FC<ContextViewProps> = ({ events, activeRunId }) => {
  const breakdown = latestContextUsage(events, activeRunId);
  const percent = contextPercent(breakdown);

  return (
    <PageShell style={surfaceStyles.page}>
      <div className="cody-page-header" style={surfaceStyles.header}>
        <div>
          <h1 style={surfaceStyles.title}>Context</h1>
          <div style={surfaceStyles.subtitle}>
            See how much of the current model’s context your conversation is using.
          </div>
        </div>
      </div>

      <div className="cody-page-body" style={surfaceStyles.scroll}>
        {activeRunId === null ? (
          <EmptyState title="Your conversation’s context" description="No active run in current session." />
        ) : (
          <div style={surfaceStyles.card}>
            <div style={{ fontSize: 12, color: "var(--cody-text-muted)" }}>
              Run <span style={{ ...surfaceStyles.mono, color: "var(--cody-text-secondary)" }}>{activeRunId}</span>
            </div>

            {breakdown === null ? (
              <>
                <div style={{ marginTop: 10, fontSize: 13, color: "var(--cody-text-secondary)" }}>
                  Context tokens: <span style={{ color: "var(--cody-link)" }}>—</span>
                </div>
                <div style={{ marginTop: 8, fontSize: 12, color: "var(--cody-text-faint)" }}>
                  Detailed breakdown not yet available from provider.
                </div>
              </>
            ) : (
              <>
                <div style={{ marginTop: 10, fontSize: 13, color: "var(--cody-text-secondary)" }}>
                  Usage:{" "}
                  <span style={{ color: "var(--cody-link)" }}>
                    {percent === null ? "—" : `${percent}%`} ({thousands(breakdown.used_tokens)}/
                    {thousands(breakdown.window_tokens)} tokens)
                  </span>
                </div>
                <div style={{ marginTop: 12, fontSize: 11, color: "var(--cody-text-faint)" }}>
                  Token distribution
                </div>
                <Row label="System prompt" tokens={breakdown.system_tokens} color="var(--cody-purple-text)" />
                <Row label="Tool declarations" tokens={breakdown.tool_tokens} color="var(--cody-link-soft)" />
                <Row
                  label="Conversation history"
                  tokens={breakdown.transcript_tokens}
                  color="var(--cody-link-soft)"
                />
              </>
            )}
          </div>
        )}
      </div>
    </PageShell>
  );
};
