import { PageShell } from "./PageShell.js";
import React, { useState } from "react";
import { useCommandActions, type CommandActions } from "../useCommandActions.js";
import { CommandFeedbackNote } from "./CommandFeedbackNote.js";
import { relativeTime } from "../time.js";
import type { InboxDeepLink, InboxEntry, InboxEntryKind } from "@codypendent/protocol";

export interface InboxViewProps {
  entries: InboxEntry[];
  onAcknowledge: (entryId: string) => void | Promise<void>;
  onDismiss: (entryId: string) => void | Promise<void>;
  onNavigate: (deepLink: InboxDeepLink, entry: InboxEntry) => void | Promise<void>;
  connected?: boolean;
  commandActions?: CommandActions;
  onRefresh?: () => void;
  /**
   * Why the inbox could not be read, when it could not be read.
   *
   * An empty `entries` array is ambiguous — it is what "nothing pending" and
   * "never answered" both look like. When this is set the view says the inbox
   * is unavailable instead of drawing the empty state, which would assert that
   * no human work is waiting when we simply do not know.
   */
  unavailable?: string | null;
}

type StateFilter = "All" | "Unread" | "Acknowledged" | "Dismissed" | "Resolved";

export const InboxView: React.FC<InboxViewProps> = ({
  entries,
  onAcknowledge,
  onDismiss,
  onNavigate,
  connected = true,
  commandActions,
  onRefresh,
  unavailable,
}) => {
  const [stateFilter, setStateFilter] = useState<StateFilter>("Unread");
  const [kindFilter, setKindFilter] = useState<string>("All");
  const localActions = useCommandActions(`${connected}`);
  const actions = commandActions ?? localActions;

  const filteredEntries = entries.filter((entry) => {
    const entryState = entry.state?.type ?? "Unread";
    if (stateFilter !== "All" && entryState !== stateFilter) {
      return false;
    }
    const entryKind = entry.kind?.type ?? "Unknown";
    if (kindFilter !== "All" && entryKind !== kindFilter) {
      return false;
    }
    return true;
  });

  const getKindBadgeColor = (kind: InboxEntryKind["type"]): { bg: string; text: string } => {
    switch (kind) {
      case "ApprovalRequest":
        return { bg: "var(--cody-warning-bg)", text: "var(--cody-attention)" };
      case "AgentQuestion":
        return { bg: "var(--cody-purple-bg)", text: "var(--cody-purple-text)" };
      case "RunCompleted":
        return { bg: "var(--cody-success-bg)", text: "var(--cody-success)" };
      case "RunFailed":
      case "RunnerFailed":
        return { bg: "var(--cody-danger-bg)", text: "var(--cody-danger-soft)" };
      case "BudgetWarning":
      case "WorkflowBlocked":
        return { bg: "var(--cody-warning-bg)", text: "var(--cody-warning)" };
      case "PluginPermissionChanged":
        return { bg: "var(--cody-info-bg)", text: "var(--cody-link)" };
      default:
        return { bg: "var(--cody-inset)", text: "var(--cody-text-muted)" };
    }
  };

  const describeDeepLink = (deepLink: InboxDeepLink): string => {
    switch (deepLink.type) {
      case "Approval":
        return `Approval ${deepLink.approval_id}`;
      case "Question":
        return `Question ${deepLink.question_id}`;
      case "Session":
        return `Session ${deepLink.session_id}`;
      case "Run":
        return `Run ${deepLink.run_id}`;
      case "Workflow":
        return `Workflow ${deepLink.workflow_id}`;
      case "Plugin":
        return `Plugin ${deepLink.plugin_id}`;
      case "Repository":
        return `Repository ${deepLink.repository_id}`;
      default:
        return "Target";
    }
  };

  return (
    <PageShell
      role="region"
      aria-label="Inbox"
      style={{
        flex: 1,
        display: "flex",
        flexDirection: "column",
        height: "100%",
        background: "var(--cody-canvas)",
        color: "var(--cody-text)",
        overflowY: "auto",
      }}
    >
      {/* Header */}
      <div
        className="cody-page-header"
        style={{
          padding: "20px 24px 16px",
          borderBottom: "1px solid var(--cody-inset)",
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          flexWrap: "wrap",
          gap: 12,
        }}
      >
        <div>
          <h1 style={{ margin: 0, fontSize: 20, fontWeight: 600 }}>Inbox</h1>
          <p style={{ margin: "4px 0 0", fontSize: 13, color: "var(--cody-text-muted)" }}>
            Decisions, questions and updates that need your attention.
          </p>
        </div>
        {onRefresh && (
          <button
            onClick={onRefresh}
            disabled={!connected}
            style={{
              padding: "6px 12px",
              background: "var(--cody-inset)",
              border: "1px solid var(--cody-border-strong)",
              borderRadius: 6,
              color: "var(--cody-text)",
              cursor: "pointer",
              fontSize: 13,
            }}
          >
            Refresh
          </button>
        )}
      </div>

      {/* Filter Toolbar */}
      <div
        style={{
          padding: "12px 24px",
          borderBottom: "1px solid var(--cody-inset)",
          display: "flex",
          alignItems: "center",
          gap: 16,
          flexWrap: "wrap",
          background: "var(--cody-panel-raised)",
        }}
      >
        {/* State filters */}
        <div style={{ display: "flex", gap: 6 }}>
          {(["All", "Unread", "Acknowledged", "Dismissed", "Resolved"] as StateFilter[]).map((state) => (
            <button
              key={state}
              onClick={() => setStateFilter(state)}
              style={{
                padding: "4px 10px",
                borderRadius: 6,
                fontSize: 12,
                fontWeight: stateFilter === state ? 600 : 400,
                background: stateFilter === state ? "var(--cody-accent-strong)" : "var(--cody-inset)",
                color: stateFilter === state ? "var(--cody-on-accent)" : "var(--cody-text-secondary)",
                border: "1px solid",
                borderColor: stateFilter === state ? "var(--cody-accent)" : "var(--cody-border-strong)",
                cursor: "pointer",
              }}
            >
              {state}
            </button>
          ))}
        </div>

        {/* Kind filter */}
        <div style={{ display: "flex", alignItems: "center", gap: 8, marginLeft: "auto" }}>
          <label htmlFor="kind-filter" style={{ fontSize: 12, color: "var(--cody-text-muted)" }}>
            Type:
          </label>
          <select
            id="kind-filter"
            value={kindFilter}
            onChange={(e) => setKindFilter(e.target.value)}
            style={{
              padding: "4px 8px",
              background: "var(--cody-inset)",
              border: "1px solid var(--cody-border-strong)",
              borderRadius: 6,
              color: "var(--cody-text)",
              fontSize: 12,
            }}
          >
            <option value="All">All Types</option>
            <option value="ApprovalRequest">Approval Request</option>
            <option value="AgentQuestion">Agent Question</option>
            <option value="RunCompleted">Run Completed</option>
            <option value="RunFailed">Run Failed</option>
            <option value="BudgetWarning">Budget Warning</option>
            <option value="WorkflowBlocked">Workflow Blocked</option>
            <option value="PluginPermissionChanged">Plugin Permission</option>
            <option value="RunnerFailed">Runner Failed</option>
          </select>
        </div>
      </div>

      {/* Entry List */}
      <div style={{ flex: 1, padding: "16px 24px" }}>
        {unavailable ? (
          /* Unread, not empty — we cannot claim there is no pending work. */
          <div
            data-testid="inbox-unavailable"
            role="status"
            style={{
              padding: "48px 24px",
              textAlign: "center",
              color: "var(--cody-warning)",
              fontSize: 14,
            }}
          >
            Inbox unavailable — {unavailable}
          </div>
        ) : filteredEntries.length === 0 ? (
          <div
            data-testid="inbox-empty"
            style={{
              padding: "48px 24px",
              textAlign: "center",
              color: "var(--cody-text-muted)",
              fontSize: 14,
            }}
          >
            {stateFilter === "All" && kindFilter === "All" ? "You’re all caught up." : "No updates match these filters."}
          </div>
        ) : (
          <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
            {filteredEntries.map((entry) => {
              const kindType = entry.kind?.type ?? "Unknown";
              const stateType = entry.state?.type ?? "Unread";
              const kindColors = getKindBadgeColor(kindType);
              const isDecision = entry.deep_link.type === "Approval" || entry.deep_link.type === "Question";
              const actionKey = `inbox:${entry.id}`;
              const result = actions.feedback[actionKey];
              const busy = result?.phase === "pending";

              return (
                <div
                  key={entry.id}
                  data-testid={`inbox-entry-${entry.id}`}
                  aria-busy={busy}
                  style={{
                    padding: 16,
                    background: "var(--cody-panel-raised)",
                    border: "1px solid var(--cody-border-strong)",
                    borderRadius: 8,
                    display: "flex",
                    flexDirection: "column",
                    gap: 10,
                  }}
                >
                  <div style={{ display: "flex", alignItems: "flex-start", justifyContent: "space-between", gap: 12 }}>
                    <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
                      <span
                        style={{
                          padding: "2px 8px",
                          borderRadius: 12,
                          fontSize: 11,
                          fontWeight: 600,
                          background: kindColors.bg,
                          color: kindColors.text,
                        }}
                      >
                        {kindType}
                      </span>
                      <span
                        style={{
                          padding: "2px 6px",
                          borderRadius: 4,
                          fontSize: 11,
                          background: "var(--cody-inset)",
                          color: "var(--cody-text-muted)",
                        }}
                      >
                        {stateType}
                      </span>
                      {entry.repository_id && (
                        <span style={{ fontSize: 11, color: "var(--cody-text-muted)" }}>repo: {entry.repository_id}</span>
                      )}
                    </div>
                    <span style={{ fontSize: 11, color: "var(--cody-text-muted)", whiteSpace: "nowrap" }}>
                      {relativeTime(entry.created_at)}
                    </span>
                  </div>

                  <div>
                    <h3 style={{ margin: "0 0 4px", fontSize: 15, fontWeight: 600, color: "var(--cody-text)" }}>
                      {entry.title}
                    </h3>
                    {entry.summary && (
                      <p style={{ margin: 0, fontSize: 13, color: "var(--cody-text-muted)", lineHeight: 1.4 }}>
                        {entry.summary}
                      </p>
                    )}
                  </div>

                  {/* Quick Actions */}
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: 8,
                      marginTop: 4,
                      paddingTop: 8,
                      borderTop: "1px solid var(--cody-inset)",
                      flexWrap: "wrap",
                    }}
                  >
                    {/* Deep link navigate */}
                    <button
                      disabled={busy || !connected}
                      onClick={() => actions.run(actionKey, () => onNavigate(entry.deep_link, entry))}
                      style={{
                        padding: "4px 10px",
                        background: "var(--cody-success-strong)",
                        border: "1px solid var(--cody-success)",
                        borderRadius: 6,
                        color: "var(--cody-on-accent)",
                        fontSize: 12,
                        fontWeight: 500,
                        cursor: "pointer",
                      }}
                      aria-label={`${isDecision ? "Review" : "Open"} ${entry.title}`}
                    >
                      {isDecision ? "Review" : "Open"} {describeDeepLink(entry.deep_link)}
                    </button>

                    {/* Acknowledge */}
                    {stateType === "Unread" && (
                      <button
                        disabled={busy || !connected}
                        onClick={() => actions.run(actionKey, () => onAcknowledge(entry.id))}
                        style={{
                          padding: "4px 10px",
                          background: "var(--cody-inset)",
                          border: "1px solid var(--cody-border-strong)",
                          borderRadius: 6,
                          color: "var(--cody-text-secondary)",
                          fontSize: 12,
                          cursor: "pointer",
                        }}
                        aria-label={`Acknowledge ${entry.title}`}
                      >
                        Acknowledge
                      </button>
                    )}

                    {/* Dismiss */}
                    {stateType !== "Dismissed" && stateType !== "Resolved" && (
                      <button
                        disabled={busy || !connected}
                        onClick={() => actions.run(actionKey, () => onDismiss(entry.id))}
                        style={{
                          padding: "4px 10px",
                          background: "var(--cody-inset)",
                          border: "1px solid var(--cody-border-strong)",
                          borderRadius: 6,
                          color: "var(--cody-text-muted)",
                          fontSize: 12,
                          cursor: "pointer",
                        }}
                        aria-label={`Dismiss ${entry.title}`}
                      >
                        Dismiss
                      </button>
                    )}
                  </div>
                  {isDecision && <p style={{ margin: 0, fontSize: 12, color: "var(--cody-text-muted)" }}>
                    Review the full request before deciding. Acknowledge and Dismiss only organize your inbox.
                  </p>}
                  <CommandFeedbackNote feedback={result} />
                </div>
              );
            })}
          </div>
        )}
      </div>
    </PageShell>
  );
};
