/**
 * The provider catalog.
 *
 * Rows are the built-in curated catalog layered with the user's
 * `providers.toml`, read in the shell by `codypendent-providers` — the same
 * catalog the CLI and TUI read. Every derived flag on a row (`available`,
 * `requires_key`, `can_list_models`, `has_key`) is the TUI's own gate; this
 * file draws them and derives nothing of its own.
 *
 * Two honesty rules shape the view:
 *
 * 1. A provider whose runtime adapter this build cannot execute is shown and
 *    DISABLED with its reason, not hidden. Hiding it would make the catalog
 *    look smaller than it is; enabling it would produce a model entry that can
 *    only fail at run time.
 * 2. Selecting the community ACP bridge is a trust decision, so it passes
 *    through a host-owned confirmation carrying the actual risk. Declining is
 *    lossless — the query and the row you were on survive it. The row cannot
 *    draw or waive its own gate.
 */
import React, { useCallback, useMemo, useRef, useState } from "react";
import { AcpConnectDialog } from "./AcpConnectDialog.js";
import { ModelDiscoveryPanel } from "./ModelDiscoveryPanel.js";
import { useLoadOnMount } from "../useLoadOnMount.js";
import { useFocusTrap } from "../useFocusTrap.js";
import { providerLabel } from "../modelPresentation.js";
import "./ModelPicker.css";

import {
  describeError,
  localConfigClient,
  shellAvailable,
  NO_SHELL,
  type LocalConfigClient,
  type ProviderRow,
  type ProvidersView,
} from "./localConfig";

export interface ProviderPickerProps {
  client?: LocalConfigClient;
  repositoryPath?: string;
  onConnected?: () => void;
  /**
   * Called with a provider the operator picked and this build can execute.
   * Never called for an unavailable row, and never called for the community
   * bridge — that path terminates in the confirmation.
   */
  onSelect?: (provider: ProviderRow) => void;
}

const PANEL: React.CSSProperties = {
  background: "var(--cody-canvas)",
  color: "var(--cody-text-secondary)",
  fontSize: 13,
  display: "flex",
  flexDirection: "column",
  height: "100%",
};

const BADGE: React.CSSProperties = {
  fontSize: 11,
  padding: "1px 6px",
  borderRadius: 10,
  background: "var(--cody-inset)",
  color: "var(--cody-text-muted)",
};

export const ProviderPicker: React.FC<ProviderPickerProps> = ({ client, onSelect, repositoryPath, onConnected }) => {
  const api = client ?? localConfigClient;
  const [view, setView] = useState<ProvidersView | null>(null);
  const [unavailable, setUnavailable] = useState<string | null>(
    shellAvailable() || client ? null : NO_SHELL,
  );
  const [acp, setAcp] = useState<ProviderRow | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [query, setQuery] = useState("");
  const [notice, setNotice] = useState<string | null>(null);
  const [configuredProviders, setConfiguredProviders] = useState<Set<string>>(() => new Set());
  /** The row awaiting a community-install trust decision, if any. */
  const [consent, setConsent] = useState<ProviderRow | null>(null);
  // The trust prompt takes focus on Decline — never on the consent button —
  // and owns Escape (see `useFocusTrap`).
  const consentRef = useRef<HTMLDivElement | null>(null);
  const declineRef = useRef<HTMLButtonElement | null>(null);
  useFocusTrap(consentRef, {
    active: consent !== null,
    initialFocus: declineRef,
    onEscape: () => setConsent(null),
  });

  const load = useCallback(async () => {
    if (!shellAvailable() && !client) {
      setUnavailable(NO_SHELL);
      return;
    }
    try {
      const [providers, models] = await Promise.allSettled([api.listProviders(), api.listModels?.()]);
      if (providers.status === "rejected") throw providers.reason;
      setView(providers.value);
      setConfiguredProviders(new Set(models.status === "fulfilled" ? models.value?.models.flatMap((model) => {
        if (model.provider === "acp") return [model.model.split(/[@#]/)[0]];
        return model.provider_id ? [model.provider_id] : [];
      }) : []));
      if (models.status === "rejected") setNotice(`Could not read connected models: ${describeError(models.reason)}`);
      setUnavailable(null);
    } catch (error) {
      setView(null);
      setUnavailable(describeError(error));
    }
  }, [api, client]);

  useLoadOnMount(load);

  const matches = useMemo(() => {
    const needle = query.trim().toLowerCase();
    const rows = view?.providers ?? [];
    if (needle.length === 0) {
      return rows;
    }
    return rows.filter(
      (row) =>
        row.id.toLowerCase().includes(needle) ||
        row.name.toLowerCase().includes(needle) ||
        row.protocol.toLowerCase().includes(needle),
    );
  }, [view, query]);

  const pick = (row: ProviderRow) => {
    if (configuredProviders.has(row.id) && !row.community_consent_required) {
      onConnected?.();
      return;
    }
    if (row.protocol === "acp" && row.available && api.connectAcp) {
      setAcp(row);
      return;
    }
    if (row.community_consent_required) {
      // The confirmation is host chrome. Nothing is installed, downloaded or
      // configured by opening it.
      setConsent(row);
      return;
    }
    if (!row.available) {
      setNotice(
        row.unusable_reason ??
          `${row.id} is catalog-only — its ${row.protocol} runtime adapter is not installed`,
      );
      return;
    }
    setNotice(null);
    onSelect?.(row);
  };

  return (
    <div style={PANEL} data-testid="provider-picker">
      <header style={{ padding: "16px 24px", borderBottom: "1px solid var(--cody-inset)" }}>
        <h2 style={{ margin: 0, fontSize: 16, fontWeight: 600 }}>Providers</h2>
        <p style={{ margin: "4px 0 0", color: "var(--cody-text-muted)", fontSize: 12 }}>
          Use installed agents and existing sign-ins, or connect another provider.
        </p>
      </header>

      {(shellAvailable() || client) && <ModelDiscoveryPanel client={api} onDiscovered={() => { void load(); onConnected?.(); }} />}

      {unavailable ? (
        <div
          role="status"
          data-testid="provider-picker-unavailable"
          style={{ padding: "32px 24px", color: "var(--cody-warning)" }}
        >
          Provider catalog unavailable — {unavailable}
        </div>
      ) : view === null ? (
        <div role="status" style={{ padding: "32px 24px", color: "var(--cody-text-muted)" }}>
          Reading the provider catalog&hellip;
        </div>
      ) : (
        <>
          <div style={{ padding: "12px 24px", borderBottom: "1px solid var(--cody-inset)" }}>
            <input
              type="search"
              value={query}
              placeholder="Filter providers"
              aria-label="Filter providers"
              onChange={(event) => setQuery(event.target.value)}
              style={{
                width: "100%",
                padding: "6px 10px",
                borderRadius: 6,
                border: "1px solid var(--cody-border-strong)",
                background: "var(--cody-canvas)",
                color: "var(--cody-text-secondary)",
                font: "inherit",
              }}
            />
          </div>

          <div style={{ padding: "8px 24px" }}>
            {api.refreshAgents && <button type="button" disabled={refreshing} onClick={async () => {
              setRefreshing(true);
              try { setView(await api.refreshAgents!()); setNotice(null); }
              catch (error) { setNotice(describeError(error)); }
              finally { setRefreshing(false); }
            }}>{refreshing ? "Refreshing agent catalog…" : "Refresh agent catalog"}</button>}
            {view.acp_unavailable && <p role="note" data-testid="provider-acp-unavailable">{view.acp_unavailable}</p>}
          </div>

          {view.warnings.map((warning) => (
            <div key={warning} role="status" style={{ padding: "4px 24px", color: "var(--cody-warning)", fontSize: 12 }}>
              {warning}
            </div>
          ))}

          <div style={{ flex: 1, overflowY: "auto", padding: "8px 16px" }}>
            {matches.length === 0 ? (
              <div data-testid="provider-picker-empty" style={{ padding: 24, color: "var(--cody-text-muted)" }}>
                {view.providers.length === 0
                  ? `No providers in ${view.providers_path} or the built-in catalog.`
                  : `No provider matches “${query}”.`}
              </div>
            ) : (
              <ul style={{ listStyle: "none", margin: 0, padding: 0, display: "flex", flexDirection: "column", gap: 6 }}>
                {matches.map((row) => (
                  <li key={row.id}>
                    <button
                      type="button"
                      onClick={() => pick(row)}
                      // Disabled rows stay focusable and clickable so their
                      // reason can be read; the click never selects them —
                      // and a screen reader hears that, instead of an
                      // ordinary actionable button.
                      aria-disabled={!row.available}
                      style={{
                        width: "100%",
                        textAlign: "left",
                        padding: "10px 12px",
                        borderRadius: 6,
                        cursor: "pointer",
                        background: "var(--cody-panel-raised)",
                        border: `1px solid ${row.community_consent_required ? "var(--cody-warning)" : "var(--cody-inset)"}`,
                        color: row.available ? "var(--cody-text-secondary)" : "var(--cody-text-muted)",
                        font: "inherit",
                      }}
                    >
                      <div style={{ display: "flex", alignItems: "baseline", gap: 8, flexWrap: "wrap" }}>
                        <span style={{ fontWeight: 600 }}>{providerLabel(row.id, row.name)}</span>
                        {configuredProviders.has(row.id) && <span style={{ ...BADGE, color: "var(--cody-success)" }}>Added to Models</span>}
                        <span style={{ ...BADGE }}>{row.protocol === "acp" ? "Agent" : row.local ? "Local models" : "API models"}</span>
                        {row.has_key && <span style={{ ...BADGE, color: "var(--cody-success)" }}>Key available</span>}
                        {row.requires_key && !row.has_key && (
                          <span style={{ ...BADGE, color: "var(--cody-warning)" }}>API key needed</span>
                        )}
                        {row.catalog_models > 0 && (
                          <span style={{ ...BADGE }}>{row.catalog_models} models</span>
                        )}
                      </div>
                      <div style={{ color: "var(--cody-text-muted)", fontSize: 12, marginTop: 2 }}>
                        {row.protocol === "acp" ? "Uses the agent’s own sign-in" : row.local ? "Runs on this computer" : "Connect using your provider account"}
                      </div>
                      {row.unusable_reason ? (
                        <div style={{ color: "var(--cody-warning)", fontSize: 12, marginTop: 4 }}>
                          {row.unusable_reason}
                        </div>
                      ) : !row.available ? (
                        // The same explanation `pick()` puts in the footer
                        // notice, ON the row — in a long list the footer can
                        // be below the fold of the very click it explains.
                        <div style={{ color: "var(--cody-warning)", fontSize: 12, marginTop: 4 }}>
                          catalog-only — its {row.protocol} runtime adapter is not installed
                        </div>
                      ) : null}
                    </button>
                    <details className="model-connection-details" style={{ margin: "6px 12px 10px" }}>
                      <summary>Connection details</summary>
                      <dl><dt>Provider ID</dt><dd><code>{row.id}</code></dd><dt>Protocol</dt><dd>{row.protocol}</dd><dt>Authentication</dt><dd>{row.auth}</dd></dl>
                    </details>
                  </li>
                ))}
              </ul>
            )}
          </div>
        </>
      )}

      {notice && (
        <div role="status" style={{ padding: "8px 24px", color: "var(--cody-warning)", fontSize: 12 }}>
          {notice}
        </div>
      )}

      {acp && <AcpConnectDialog provider={acp} repositoryPath={repositoryPath} client={api}
        onClose={() => setAcp(null)} onConnected={() => { setAcp(null); setNotice(`Connected ${acp.name}. Its profiles are available in Models.`); onConnected?.(); }} />}

      {consent && (
        /* Host-owned trust confirmation. The evidence the operator is consenting
           to travels ON the prompt, and declining restores the list exactly as
           it was — the filter and the row are untouched. */
        <div
          ref={consentRef}
          role="alertdialog"
          aria-label="Community ACP bridge"
          data-testid="community-acp-confirm"
          style={{
            borderTop: "1px solid var(--cody-warning)",
            background: "var(--cody-warning-bg)",
            padding: "16px 24px",
          }}
        >
          <h3 style={{ margin: 0, fontSize: 14, color: "var(--cody-warning)" }}>
            {consent.name} — third-party trust decision
          </h3>
          <p style={{ margin: "8px 0 0", color: "var(--cody-text-secondary)", fontSize: 12 }}>
            {consent.community_consent_detail}
          </p>
          {consent.unusable_reason && (
            <p style={{ margin: "8px 0 0", color: "var(--cody-text-muted)", fontSize: 12 }}>
              {consent.unusable_reason}
            </p>
          )}
          <div style={{ display: "flex", gap: 8, marginTop: 12 }}>
            <button
              type="button"
              onClick={() => {
                // Confirming cannot install here, and says so rather than
                // appearing to succeed.
                setNotice(
                  consent.unusable_reason ??
                    `${consent.id} cannot be installed from the desktop shell`,
                );
                setConsent(null);
              }}
              style={{
                padding: "6px 12px",
                borderRadius: 6,
                border: "1px solid var(--cody-warning)",
                background: "transparent",
                color: "var(--cody-warning)",
                cursor: "pointer",
                font: "inherit",
              }}
            >
              I understand the risk
            </button>
            <button
              type="button"
              ref={declineRef}
              onClick={() => setConsent(null)}
              style={{
                padding: "6px 12px",
                borderRadius: 6,
                border: "1px solid var(--cody-border-strong)",
                background: "transparent",
                color: "var(--cody-text-secondary)",
                cursor: "pointer",
                font: "inherit",
              }}
            >
              Cancel
            </button>
          </div>
        </div>
      )}
    </div>
  );
};

export default ProviderPicker;
