import { PageShell } from "./PageShell.js";
/**
 * Remote UI plugins — the TUI's `Overlay::UiPlugins` and its four confirms.
 *
 * The rule this view enforces (`crates/tui/src/state.rs`): "Plugin code can
 * never draw or intercept its own trust or revocation controls." Every verb
 * below is host-owned chrome rendered by this component, and every one of the
 * four trust transitions goes through a confirmation:
 *
 * - ENABLE is a trust transition, so clicking it opens a confirm showing the
 *   daemon's `update_permission_diff`, or the explicit fallback sentence when
 *   the daemon supplied none. Only confirming sends `EnableUiPlugin`.
 * - APPROVE carries the EXACT daemon-issued receipt AND the exact
 *   daemon-supplied permission diff onto the prompt, so the decisive moment
 *   never loses the evidence being consented to. With no pending update it
 *   refuses — it does not fabricate a receipt.
 * - REJECT carries the same receipt.
 * - REVOKE always confirms.
 *
 * Smoke-testing is not a trust transition and fires directly.
 */
import React, { useState } from "react";
import type { Loaded, UiPluginLifecycleStatus } from "./knowledgeTransport.js";
import { ConfirmPanel, Field, SurfaceBody, surfaceButton, surfaceStyles } from "./surfaceChrome.js";

/** Shown when the daemon reported no permission delta for an enable. */
export const NO_PENDING_EXPANSION =
  "No pending permission expansion. Enabling grants the permissions declared by the verified installed package.";

/** Shown on an approve prompt when the daemon supplied no diff with the receipt. */
export const NO_PERMISSION_CHANGES = "No permission changes reported.";

/** The refusal when approve/reject is asked for and there is no pending update. */
export const NO_PENDING_UPDATE = "selected plugin has no pending update";

/**
 * Appended when an enable is sent: the remote-UI document stream is not wired
 * in this build (the shell feeds the renderer an empty map), so an enabled
 * plugin draws nothing here yet. Said in as many words so the daemon's success
 * notice is not read as "the plugin is now visible".
 */
export const ENABLE_NOT_RENDERED =
  "Plugin surfaces are not rendered in this build — remote-UI rendering arrives in a later build, so enabling changes the daemon's state but nothing will appear here yet.";

/** The scopes an enable can be granted at, narrowest first. */
export const PLUGIN_SCOPES = ["session", "repository", "user"] as const;

export interface PluginsViewProps {
  plugins: Loaded<UiPluginLifecycleStatus>;
  onRefresh?: () => void;
  onSmokeTest?: (pluginId: string) => void;
  onEnable?: (pluginId: string, scope: string) => void;
  onApprove?: (pluginId: string, approvalReceipt: string) => void;
  onReject?: (pluginId: string, approvalReceipt: string) => void;
  onRevoke?: (pluginId: string) => void;
  /** The last daemon notice, shown verbatim. */
  notice?: string | null;
}

type Confirm =
  | { kind: "enable"; pluginId: string; scope: string; permissionSummary: string }
  | { kind: "approve"; pluginId: string; receipt: string; permissionDiff: string }
  | { kind: "reject"; pluginId: string; receipt: string }
  | { kind: "revoke"; pluginId: string };

export const PluginsView: React.FC<PluginsViewProps> = ({
  plugins,
  onRefresh,
  onSmokeTest,
  onEnable,
  onApprove,
  onReject,
  onRevoke,
  notice,
}) => {
  const [confirm, setConfirm] = useState<Confirm | null>(null);
  const [scope, setScope] = useState<Record<string, string>>({});
  const [refusal, setRefusal] = useState<string | null>(null);
  /** Set when an enable is sent, until the next action replaces it. */
  const [enableNote, setEnableNote] = useState<string | null>(null);

  const beginApprove = (plugin: UiPluginLifecycleStatus) => {
    const receipt = plugin.updateApprovalReceipt;
    if (!receipt) {
      setRefusal(NO_PENDING_UPDATE);
      return;
    }
    setRefusal(null);
    setConfirm({
      kind: "approve",
      pluginId: plugin.id,
      receipt,
      permissionDiff: plugin.updatePermissionDiff ?? NO_PERMISSION_CHANGES,
    });
  };

  const beginReject = (plugin: UiPluginLifecycleStatus) => {
    const receipt = plugin.updateApprovalReceipt;
    if (!receipt) {
      setRefusal(NO_PENDING_UPDATE);
      return;
    }
    setRefusal(null);
    setConfirm({ kind: "reject", pluginId: plugin.id, receipt });
  };

  return (
    <PageShell style={surfaceStyles.page}>
      <div className="cody-page-header" style={surfaceStyles.header}>
        <div>
          <h1 style={surfaceStyles.title}>Extensions</h1>
          <div style={surfaceStyles.subtitle}>
            Add capabilities to Codypendent and review what each extension can access.
          </div>
        </div>
        {onRefresh && (
          <button style={surfaceButton()} onClick={onRefresh}>
            Refresh
          </button>
        )}
      </div>

      <div className="cody-page-body" style={surfaceStyles.scroll}>
        {notice && (
          <div role="status" style={{ ...surfaceStyles.card, color: "var(--cody-success-text)", borderColor: "var(--cody-success-strong)" }}>
            {notice}
          </div>
        )}
        {enableNote && (
          <div role="status" style={{ ...surfaceStyles.card, color: "var(--cody-success-text)", borderColor: "var(--cody-success-strong)" }}>
            {enableNote}
          </div>
        )}
        {refusal && (
          <div role="alert" style={{ ...surfaceStyles.card, color: "var(--cody-danger-text)", borderColor: "var(--cody-danger)" }}>
            {refusal}
          </div>
        )}

        {confirm?.kind === "enable" && (
          <ConfirmPanel
            title={`Enable ${confirm.pluginId} at ${confirm.scope} scope?`}
            evidence={confirm.permissionSummary}
            confirmLabel="Enable"
            onConfirm={() => {
              onEnable?.(confirm.pluginId, confirm.scope);
              setEnableNote(ENABLE_NOT_RENDERED);
              setConfirm(null);
            }}
            onCancel={() => setConfirm(null)}
          />
        )}
        {confirm?.kind === "approve" && (
          <ConfirmPanel
            title={`Approve the pending update to ${confirm.pluginId}?`}
            evidence={`receipt: ${confirm.receipt}\n\npermission diff:\n${confirm.permissionDiff}`}
            confirmLabel="Approve update"
            onConfirm={() => {
              onApprove?.(confirm.pluginId, confirm.receipt);
              setConfirm(null);
            }}
            onCancel={() => setConfirm(null)}
          />
        )}
        {confirm?.kind === "reject" && (
          <ConfirmPanel
            title={`Reject the pending update to ${confirm.pluginId}?`}
            evidence={`receipt: ${confirm.receipt}`}
            confirmLabel="Reject update"
            onConfirm={() => {
              onReject?.(confirm.pluginId, confirm.receipt);
              setConfirm(null);
            }}
            onCancel={() => setConfirm(null)}
          />
        )}
        {confirm?.kind === "revoke" && (
          <ConfirmPanel
            title={`Revoke ${confirm.pluginId} and tear down its workers?`}
            confirmLabel="Revoke"
            onConfirm={() => {
              onRevoke?.(confirm.pluginId);
              setConfirm(null);
            }}
            onCancel={() => setConfirm(null)}
          />
        )}

        <SurfaceBody
          emptyTitle="Make Codypendent your own"
          onRetry={onRefresh}
          status={plugins.status}
          detail={plugins.detail}
          count={plugins.items.length}
          emptyMessage="No UI plugins are installed."
        >
          {plugins.items.map((plugin) => {
            const chosenScope = scope[plugin.id] ?? PLUGIN_SCOPES[0];
            const hasPendingUpdate = Boolean(plugin.updateApprovalReceipt);
            return (
              <div key={plugin.id} style={surfaceStyles.card}>
                <div style={{ display: "flex", justifyContent: "space-between", gap: 12 }}>
                  <span style={{ fontSize: 13, fontWeight: 600, color: "var(--cody-text)" }}>
                    {plugin.id}
                  </span>
                  <span style={{ ...surfaceStyles.mono, color: "var(--cody-text-muted)" }}>{plugin.version}</span>
                </div>
                <div style={{ marginTop: 8 }}>
                  <Field label="state" value={plugin.state} />
                  <Field
                    label="enabled scope"
                    value={plugin.enabledScope ?? "not enabled"}
                  />
                  <Field
                    label="pending update"
                    value={hasPendingUpdate ? "yes" : "none"}
                  />
                </div>
                {plugin.updatePermissionDiff && (
                  <pre
                    style={{
                      ...surfaceStyles.mono,
                      whiteSpace: "pre-wrap",
                      wordBreak: "break-word",
                      color: "var(--cody-warning-text)",
                      background: "var(--cody-canvas)",
                      border: "1px solid var(--cody-border-strong)",
                      borderRadius: 6,
                      padding: 10,
                      marginTop: 10,
                      marginBottom: 0,
                      // Bounded, scrollable within the card: a large diff must
                      // not push the decision buttons below it off screen.
                      maxHeight: 260,
                      overflowY: "auto",
                    }}
                  >
                    {plugin.updatePermissionDiff}
                  </pre>
                )}

                <div style={{ display: "flex", gap: 8, marginTop: 10, flexWrap: "wrap", alignItems: "center" }}>
                  {onSmokeTest && (
                    <button style={surfaceButton()} onClick={() => onSmokeTest(plugin.id)}>
                      Smoke test
                    </button>
                  )}
                  {onEnable && (
                    <>
                      <select
                        value={chosenScope}
                        aria-label={`Enable scope for ${plugin.id}`}
                        onChange={(event) =>
                          setScope({ ...scope, [plugin.id]: event.target.value })
                        }
                        style={{
                          background: "var(--cody-canvas)",
                          border: "1px solid var(--cody-border-strong)",
                          borderRadius: 6,
                          color: "var(--cody-text)",
                          fontSize: 12,
                          padding: "4px 8px",
                        }}
                      >
                        {PLUGIN_SCOPES.map((value) => (
                          <option key={value} value={value}>
                            {value}
                          </option>
                        ))}
                      </select>
                      <button
                        style={surfaceButton()}
                        onClick={() =>
                          setConfirm({
                            kind: "enable",
                            pluginId: plugin.id,
                            scope: chosenScope,
                            permissionSummary:
                              plugin.updatePermissionDiff ?? NO_PENDING_EXPANSION,
                          })
                        }
                      >
                        Enable…
                      </button>
                    </>
                  )}
                  {onApprove && (
                    <button style={surfaceButton()} onClick={() => beginApprove(plugin)}>
                      Approve update…
                    </button>
                  )}
                  {onReject && (
                    <button style={surfaceButton()} onClick={() => beginReject(plugin)}>
                      Reject update…
                    </button>
                  )}
                  {onRevoke && (
                    <button
                      style={surfaceButton("danger")}
                      onClick={() => setConfirm({ kind: "revoke", pluginId: plugin.id })}
                    >
                      Revoke…
                    </button>
                  )}
                </div>
              </div>
            );
          })}
        </SurfaceBody>
      </div>
    </PageShell>
  );
};
