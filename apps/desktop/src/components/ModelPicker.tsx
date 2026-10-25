/**
 * Configured models: list, choose one to use, add one, remove one.
 *
 * Rows come from `models.toml`, parsed in the shell by the same
 * `codypendent_runtime::models::load_models` the daemon and the CLI use. A
 * missing file is an empty list ("nothing configured yet"); a file that exists
 * and does not parse is an error, and the two are drawn differently on purpose.
 *
 * What is NOT here, and why: the TUI's model picker carries a readiness badge
 * (Ready / Unverified / Unavailable) computed by probing local endpoints and
 * resolving credentials through `ModelRegistry`. The shell does not compile
 * that machinery — it configures models; the daemon runs them — so no readiness
 * is shown at all rather than a confident badge nothing measured. Credential
 * PRESENCE is real and is shown; the key value is not, and never crosses the
 * bridge.
 *
 * Choosing a model sends nothing to the daemon. It sets `StartRun.model`,
 * exactly as `pending_model` does in the TUI, and the shell refuses one that
 * `models.toml` does not contain.
 *
 * It reads as "use this now" because for everything the operator does next,
 * that is what it is. The one honest caveat is stated in the panel rather than
 * in the button: no protocol command switches the model of a run that is
 * already in flight — there is no `SetRunModel` in `CommandBody` — so such a
 * run finishes on the model it started with. Labelling the button "Use next"
 * made every selection look deferred, including the overwhelmingly common case
 * where nothing is running at all.
 */
import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useLoadOnMount } from "../useLoadOnMount.js";
import { useFocusTrap } from "../useFocusTrap.js";
import { ModelDiscoveryPanel } from "./ModelDiscoveryPanel.js";
import { modelLabel, modelProviderLabel, modelVersionLabel, providerLabel } from "../modelPresentation.js";
import "./ModelPicker.css";

import {
  describeError,
  describeKeyStatus,
  describeReadiness,
  localConfigClient,
  noticeStyle,
  shellAvailable,
  NO_SHELL,
  type CatalogModelsView,
  type LocalConfigClient,
  type ModelReadinessRow,
  type ModelRow,
  type ModelsView,
  type Notice,
  type ProviderRow,
} from "./localConfig";

export interface ModelPickerProps {
  client?: LocalConfigClient;
  /** Told the pinned model after the shell accepted it, for a status line. */
  onModelPinned?: (modelId: string | null) => void;
  /**
   * Open the add-model flow already on this provider.
   *
   * The Providers page lists what is available but had nowhere to send a
   * selection — it rendered with no `onSelect` at all, so clicking a provider
   * optional-chained into nothing and the page looked inert. The credential and
   * model form already existed here; this is the route to it.
   */
  initialProvider?: ProviderRow | null;
  /** Told once `initialProvider` has been consumed, so it is not reopened. */
  onInitialProviderUsed?: () => void;
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

const BUTTON: React.CSSProperties = {
  padding: "4px 10px",
  borderRadius: 6,
  border: "1px solid var(--cody-border-strong)",
  background: "transparent",
  color: "var(--cody-text-secondary)",
  cursor: "pointer",
  font: "inherit",
  fontSize: 12,
};

/** The host part of a base URL, for a label. A trim, not a URL parse. */
function endpointHost(baseUrl: string): string {
  const rest = (baseUrl.includes("://") ? baseUrl.split("://")[1] : baseUrl).replace(/^\/+/, "");
  const host = rest.split("/")[0];
  return host.length > 0 ? host : baseUrl;
}

export const ModelPicker: React.FC<ModelPickerProps> = ({
  client,
  onModelPinned,
  initialProvider,
  onInitialProviderUsed,
}) => {
  const api = client ?? localConfigClient;
  const [view, setView] = useState<ModelsView | null>(null);
  const [unavailable, setUnavailable] = useState<string | null>(
    shellAvailable() || client ? null : NO_SHELL,
  );
  const [query, setQuery] = useState("");
  const [notice, setNotice] = useState<Notice | null>(null);
  const [removing, setRemoving] = useState<ModelRow | null>(null);
  const [adding, setAdding] = useState(Boolean(initialProvider));
  // The removal confirmation takes focus (on Cancel, never on Remove) and
  // owns Escape: after clicking Remove, focus used to stay on that button so
  // Enter fired it again, and Escape navigated the whole view away.
  const removeDialogRef = useRef<HTMLDivElement | null>(null);
  const removeCancelRef = useRef<HTMLButtonElement | null>(null);
  useFocusTrap(removeDialogRef, {
    active: removing !== null,
    initialFocus: removeCancelRef,
    onEscape: () => setRemoving(null),
  });
  /**
   * Readiness per model id, as the shell computed it — the TUI's picker
   * badges. `"checking"` while a read or a Test is in flight; absent when the
   * shell does not offer the command, in which case no badge is drawn rather
   * than a guessed one.
   */
  const [readiness, setReadiness] = useState<Map<string, ModelReadinessRow | "checking">>(
    () => new Map(),
  );
  /**
   * A monotonic clock ordering readiness writes, and the tick at which each
   * row's Test last resolved.
   *
   * Refs, not state: ordering must be readable inside the state updater and
   * must never itself cause a render.
   */
  const readinessClock = useRef(0);
  const testedAt = useRef(new Map<string, number>());

  const loadReadiness = useCallback(async () => {
    if (!api.listModelReadiness) {
      return;
    }
    const startedAt = ++readinessClock.current;
    try {
      const rows = await api.listModelReadiness();
      setReadiness((current) => {
        // Rebuilt from the returned rows, so a removed model keeps no badge —
        // but never over a per-row Test that resolved after this read began, or
        // one still running. The Test asked the PROVIDER; this read only looked
        // at stored credentials, so letting it land on top reverted a proven
        // Ready to Unverified beside a notice saying the test had succeeded.
        const next = new Map<string, ModelReadinessRow | "checking">();
        for (const row of rows) {
          const existing = current.get(row.id);
          const newer =
            existing === "checking" || (testedAt.current.get(row.id) ?? 0) > startedAt;
          next.set(row.id, newer && existing ? existing : row);
        }
        return next;
      });
    } catch (error) {
      // No badge is better than a wrong one; the reason goes to the notice.
      setNotice({ tone: "error", text: `could not check model readiness: ${describeError(error)}` });
    }
  }, [api]);

  /** Ask the provider, over the network, because the operator pressed Test. */
  const test = async (row: ModelRow) => {
    if (!api.modelReadiness) {
      return;
    }
    setReadiness((current) => new Map(current).set(row.id, "checking"));
    try {
      const verdict = await api.modelReadiness(row.id, true);
      // Stamped at RESOLUTION: a bulk read that began before this point must
      // not overwrite the verdict below.
      testedAt.current.set(row.id, ++readinessClock.current);
      setReadiness((current) => new Map(current).set(row.id, verdict));
      setNotice({
        tone: verdict.readiness.state === "unavailable" ? "error" : "ok",
        text: `${modelLabel(row.id)}: ${verdict.readiness.state} — ${("detail" in verdict.readiness ? verdict.readiness.detail : "The daemon returned an unknown readiness state")}`,
      });
    } catch (error) {
      setReadiness((current) => {
        const next = new Map(current);
        next.delete(row.id);
        return next;
      });
      setNotice({ tone: "error", text: `Could not test ${modelLabel(row.id)}: ${describeError(error)}` });
    }
  };

  const load = useCallback(async () => {
    if (!shellAvailable() && !client) {
      setUnavailable(NO_SHELL);
      return;
    }
    try {
      setView(await api.listModels());
      setUnavailable(null);
      // Badges fill in after the list draws; a slow local endpoint must not
      // hold the whole page.
      void loadReadiness();
    } catch (error) {
      // Not an empty list: `models.toml` could not be read.
      setView(null);
      setUnavailable(describeError(error));
    }
  }, [api, client, loadReadiness]);

  useLoadOnMount(load);

  const matches = useMemo(() => {
    const terms = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
    const rows = view?.models ?? [];
    if (terms.length === 0) {
      return rows;
    }
    return rows.filter(
      (row) => {
        const text = [row.id, row.model, modelLabel(row.id), modelProviderLabel(row.id, row.provider_id, row.provider)].join(" ").toLowerCase();
        return terms.every((term) => text.includes(term));
      },
    );
  }, [view, query]);

  const pin = async (modelId: string | null) => {
    try {
      await api.setRunModel(modelId);
      setNotice({
        tone: "ok",
        text: modelId === null ? "Automatic model selection enabled." : `Now using ${modelLabel(modelId)}.`,
      });
      onModelPinned?.(modelId);
    } catch (error) {
      setNotice({ tone: "error", text: `could not pin model: ${describeError(error)}` });
    }
    await load();
  };

  const confirmRemove = async () => {
    if (!removing) {
      return;
    }
    try {
      await api.removeModel(removing.id);
      setNotice({ tone: "ok", text: `Removed ${modelLabel(removing.id)}.` });
    } catch (error) {
      setNotice({ tone: "error", text: `could not remove model: ${describeError(error)}` });
    } finally {
      setRemoving(null);
    }
    await load();
  };

  return (
    <div style={PANEL} data-testid="model-picker">
      <header style={{ padding: "16px 24px", borderBottom: "1px solid var(--cody-inset)" }}>
        <h2 style={{ margin: 0, fontSize: 16, fontWeight: 600 }}>Models</h2>
        <p style={{ margin: "4px 0 0", color: "var(--cody-text-muted)", fontSize: 12 }}>
          Choose a model for your next task. Installed agents and local models are added automatically.
        </p>
      </header>

      {(shellAvailable() || client) && <ModelDiscoveryPanel client={api} onDiscovered={() => { void load(); }} />}

      {unavailable ? (
        <div
          role="status"
          data-testid="model-picker-unavailable"
          style={{ padding: "32px 24px", color: "var(--cody-warning)" }}
        >
          Models unavailable — {unavailable}
        </div>
      ) : view === null ? (
        <div role="status" style={{ padding: "32px 24px", color: "var(--cody-text-muted)" }}>
          Reading models&hellip;
        </div>
      ) : (
        <>
          <div style={{ padding: "12px 24px", display: "flex", gap: 8, borderBottom: "1px solid var(--cody-inset)" }}>
            <input
              type="search"
              value={query}
              placeholder="Filter models"
              onChange={(event) => setQuery(event.target.value)}
              style={{
                flex: 1,
                padding: "6px 10px",
                borderRadius: 6,
                border: "1px solid var(--cody-border-strong)",
                background: "var(--cody-canvas)",
                color: "var(--cody-text-secondary)",
                font: "inherit",
              }}
            />
            <button type="button" style={BUTTON} onClick={() => setAdding(true)}>
              Add model
            </button>
            {view.pinned && (
              <button type="button" style={BUTTON} onClick={() => void pin(null)}>
                Use automatic selection
              </button>
            )}
          </div>

          {view.warnings.map((warning) => (
            <div key={warning} role="status" style={{ padding: "4px 24px", color: "var(--cody-warning)", fontSize: 12 }}>
              {warning}
            </div>
          ))}

          <div style={{ flex: 1, overflowY: "auto", padding: "8px 16px" }}>
            {matches.length === 0 ? (
              <div data-testid="model-picker-empty" style={{ padding: 24, color: "var(--cody-text-muted)" }}>
                {view.models.length === 0
                  ? view.configured
                    ? "No models found yet. Scan for installed tools above, or add a provider."
                    : "Your models will appear here after Codypendent finds an installed agent or a running local model."
                  : `No model matches “${query}”.`}
              </div>
            ) : (
              <ul style={{ listStyle: "none", margin: 0, padding: 0, display: "flex", flexDirection: "column", gap: 6 }}>
                {matches.map((row) => {
                  const status = row.provider === "acp"
                    ? { glyph: "", label: "Agent account", color: "var(--cody-text-muted)" }
                    : { ...describeKeyStatus(row.key), label: row.key.state === "stored" || row.key.state === "env" ? "Key available" : describeKeyStatus(row.key).label };
                  const pinned = view.pinned === row.id;
                  const verdict = readiness.get(row.id);
                  const badge =
                    verdict === undefined
                      ? null
                      : verdict === "checking"
                        ? { glyph: "…", label: "checking", color: "var(--cody-text-muted)", detail: "" }
                        : { ...describeReadiness(verdict.readiness), detail: ("detail" in verdict.readiness ? verdict.readiness.detail : "The daemon returned an unknown readiness state") };
                  return (
                    <li
                      key={row.id}
                      className="model-picker-row"
                      style={{
                        padding: "10px 12px",
                        borderRadius: 6,
                        background: "var(--cody-panel-raised)",
                        border: `1px solid ${pinned ? "var(--cody-link)" : "var(--cody-inset)"}`,
                        display: "flex",
                        alignItems: "center",
                        gap: 12,
                      }}
                    >
                      <div style={{ flex: 1, minWidth: 0 }}>
                        <div style={{ display: "flex", alignItems: "baseline", gap: 8, flexWrap: "wrap" }}>
                          <span style={{ fontWeight: 600 }}>{modelLabel(row.id)}</span>
                          {pinned && (
                            <span style={{ ...BADGE, color: "var(--cody-link)" }}>Selected</span>
                          )}
                          {(row.provider === "acp" || row.key.state !== "missing") && <span style={{ ...BADGE, color: status.color }}>
                            {status.glyph} {status.label}
                          </span>}
                          {badge && badge.label !== "unverified" && (
                            <span
                              data-testid={`model-readiness-${row.id}`}
                              title={badge.detail}
                              style={{ ...BADGE, color: badge.color }}
                            >
                              {badge.glyph} {badge.label}
                            </span>
                          )}
                        </div>
                        {badge && badge.label === "unavailable" && (
                          <div style={{ color: "var(--cody-danger-soft)", fontSize: 12, marginTop: 2 }}>{badge.detail}</div>
                        )}
                        <div style={{ color: "var(--cody-text-muted)", fontSize: 12, marginTop: 2 }}>
                          {modelProviderLabel(row.id, row.provider_id, row.provider)}
                          {row.context_tokens !== null && ` · ${new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 }).format(row.context_tokens)} context`}
                        </div>
                        <details className="model-connection-details">
                          <summary>Connection details</summary>
                          <dl>
                            <dt>Model ID</dt><dd><code>{row.id}</code></dd>
                            <dt>Provider model</dt><dd><code>{row.model}</code></dd>
                            {row.base_url.trim() && <><dt>Endpoint</dt><dd>{endpointHost(row.base_url)}</dd></>}
                            <dt>Credentials</dt><dd>{row.provider === "acp" ? "Uses the installed agent’s account" : describeKeyStatus(row.key).label}</dd>
                            {badge && <><dt>Check</dt><dd>{badge.label === "unverified" ? "Not tested" : badge.label}</dd></>}
                          </dl>
                        </details>
                      </div>
                      {api.modelReadiness && (
                        <button
                          type="button"
                          style={BUTTON}
                          disabled={verdict === "checking"}
                          title="Ask the provider whether this model answers (uses the network)"
                          onClick={() => void test(row)}
                        >
                          Test
                        </button>
                      )}
                      <button type="button" style={BUTTON} disabled={pinned} onClick={() => void pin(row.id)}>
                        {pinned ? "In use" : "Use"}
                      </button>
                      <button
                        type="button"
                        style={{ ...BUTTON, color: "var(--cody-danger-soft)" }}
                        onClick={() => setRemoving(row)}
                      >
                        Remove
                      </button>
                    </li>
                  );
                })}
              </ul>
            )}
          </div>
        </>
      )}

      {removing && (
        <div
          ref={removeDialogRef}
          role="alertdialog"
          aria-label="Remove model"
          data-testid="model-remove-confirm"
          style={{ borderTop: "1px solid var(--cody-inset)", padding: "16px 24px" }}
        >
          <p style={{ margin: 0, fontSize: 13 }}>
            Remove <strong>{modelLabel(removing.id)}</strong>? Any credentials saved specifically for this model are also removed.
          </p>
          <div style={{ display: "flex", gap: 8, marginTop: 12 }}>
            <button
              type="button"
              onClick={() => void confirmRemove()}
              style={{ ...BUTTON, border: "1px solid var(--cody-danger)", background: "var(--cody-danger)", color: "var(--cody-on-accent)" }}
            >
              Remove
            </button>
            <button type="button" ref={removeCancelRef} style={BUTTON} onClick={() => setRemoving(null)}>
              Cancel
            </button>
          </div>
        </div>
      )}

      {adding && (
        <AddModelFlow
          api={api}
          initialProvider={initialProvider ?? null}
          onCancel={() => {
            setAdding(false);
            onInitialProviderUsed?.();
          }}
          onAdded={async (id) => {
            setAdding(false);
            onInitialProviderUsed?.();
            setNotice({ tone: "ok", text: `added ${id} to models.toml` });
            await load();
          }}
        />
      )}

      {notice && (
        <div role={notice.tone === "error" ? "alert" : "status"} style={noticeStyle(notice)}>
          {notice.text}
        </div>
      )}
    </div>
  );
};

/**
 * The add-model flow: provider, then model, then key, then the id it will be
 * selected by. The same order as the TUI's five-step overlay chain.
 *
 * The model list offered is the provider catalog's CURATED rows — real data,
 * read offline. The TUI can additionally GET `{base_url}/models` with the
 * provider's key; the shell does not, so the list is labelled as the catalog's
 * rather than presented as exhaustive, and any model name can still be typed.
 *
 * Every refusal that matters is enforced in the shell (blank id, blank key
 * treated as absent, corrupt `auth.json` aborting before `models.toml` is
 * touched, a provider whose adapter is not installed). What is duplicated here
 * is only what stops a pointless round trip.
 */
const AddModelFlow: React.FC<{
  api: LocalConfigClient;
  onCancel: () => void;
  onAdded: (displayId: string) => void | Promise<void>;
  initialProvider?: ProviderRow | null;
}> = ({ api, onCancel, onAdded, initialProvider = null }) => {
  const [providers, setProviders] = useState<ProviderRow[] | null>(null);
  const [providersUnavailable, setProvidersUnavailable] = useState<string | null>(null);
  const [provider, setProvider] = useState<ProviderRow | null>(initialProvider);
  const [catalog, setCatalog] = useState<CatalogModelsView | null>(null);
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const [model, setModel] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [displayId, setDisplayId] = useState("");
  const [displayIdTouched, setDisplayIdTouched] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const view = await api.listProviders();
        if (!cancelled) {
          // The community bridge is a trust decision handled by the provider
          // catalog view, and this build cannot install it — it is not an
          // option in an add flow.
          setProviders(
            view.providers.filter((row) => row.available && !row.community_consent_required),
          );
        }
      } catch (loadError) {
        if (!cancelled) {
          setProviders(null);
          setProvidersUnavailable(describeError(loadError));
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [api]);

  useEffect(() => {
    if (!provider) {
      return;
    }
    let cancelled = false;
    setCatalog(null);
    setCatalogError(null);
    void (async () => {
      try {
        const view = await api.listCatalogModels(provider.id);
        if (!cancelled) {
          setCatalog(view);
        }
      } catch (loadError) {
        if (!cancelled) {
          setCatalogError(describeError(loadError));
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [api, provider]);

  // The TUI's default display id is `<provider>/<model>`; a typed one wins.
  const effectiveId =
    displayIdTouched || displayId.length > 0
      ? displayId
      : provider && model
        ? `${provider.id}/${model}`
        : "";

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!provider) {
      setError("choose a provider first");
      return;
    }
    if (model.trim().length === 0) {
      setError("model name must not be blank");
      return;
    }
    if (effectiveId.trim().length === 0) {
      setError("model id must not be blank");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const contextTokens =
        catalog?.models.find((row) => row.id === model.trim())?.context_tokens ?? null;
      await api.addModel({
        displayId: effectiveId.trim(),
        providerId: provider.id,
        model: model.trim(),
        // A blank key is `null`: the shell treats blank exactly as absent, and
        // sending an empty string would risk shadowing a valid api_key_env.
        apiKey: apiKey.trim().length > 0 ? apiKey : null,
        contextTokens,
      });
      // Cleared before anything else can observe it.
      setApiKey("");
      await onAdded(effectiveId.trim());
    } catch (addError) {
      setApiKey("");
      setError(describeError(addError));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form
      onSubmit={submit}
      data-testid="add-model-flow"
      style={{ borderTop: "1px solid var(--cody-inset)", padding: "16px 24px", display: "flex", flexDirection: "column", gap: 10 }}
    >
      <h3 style={{ margin: 0, fontSize: 14 }}>Add a model</h3>

      {providersUnavailable ? (
        <div role="status" style={{ color: "var(--cody-warning)", fontSize: 12 }}>
          Provider catalog unavailable — {providersUnavailable}
        </div>
      ) : providers === null ? (
        <div role="status" style={{ color: "var(--cody-text-muted)", fontSize: 12 }}>
          Reading the provider catalog&hellip;
        </div>
      ) : (
        <label style={{ fontSize: 12, color: "var(--cody-text-muted)" }}>
          Provider
          <select
            value={provider?.id ?? ""}
            onChange={(event) => {
              const next = providers.find((row) => row.id === event.target.value) ?? null;
              setProvider(next);
              setModel("");
              // A key belongs to the provider it was typed for; carrying it
              // into another provider's entry would submit it masked, unseen.
              setApiKey("");
            }}
            style={{
              display: "block",
              width: "100%",
              marginTop: 4,
              padding: "6px 10px",
              borderRadius: 6,
              border: "1px solid var(--cody-border-strong)",
              background: "var(--cody-canvas)",
              color: "var(--cody-text-secondary)",
              font: "inherit",
            }}
          >
            <option value="">Choose a provider…</option>
            {providers.map((row) => (
              <option key={row.id} value={row.id}>
                {providerLabel(row.id, row.name)}{row.local ? " · Local" : ""}
              </option>
            ))}
          </select>
        </label>
      )}

      {provider && (
        <>
          <label style={{ fontSize: 12, color: "var(--cody-text-muted)" }}>
            Model
            <input
              list="catalog-models"
              value={model}
              onChange={(event) => setModel(event.target.value)}
              placeholder="provider-side model name"
              style={{
                display: "block",
                width: "100%",
                marginTop: 4,
                padding: "6px 10px",
                borderRadius: 6,
                border: "1px solid var(--cody-border-strong)",
                background: "var(--cody-canvas)",
                color: "var(--cody-text-secondary)",
                font: "inherit",
              }}
            />
          </label>
          <datalist id="catalog-models">
            {(catalog?.models ?? []).map((row) => (
              <option key={row.id} value={row.id}>
                {row.name ?? modelVersionLabel(row.id)}
              </option>
            ))}
          </datalist>
          {catalogError ? (
            <div role="status" style={{ color: "var(--cody-warning)", fontSize: 12 }}>
              Catalog models unavailable — {catalogError}
            </div>
          ) : catalog ? (
            <div style={{ color: "var(--cody-text-muted)", fontSize: 12 }}>
              {catalog.models.length === 0
                ? "The catalog ships no curated rows for this provider — type the model name."
                : `${catalog.models.length} curated rows offered. `}
              {catalog.live_listing_unavailable}
            </div>
          ) : null}

          {/* The key step exists only when the provider needs one and none
              already resolves — the TUI's `requires_key && !has_key` branch. */}
          {provider.requires_key && !provider.has_key && (
            <label style={{ fontSize: 12, color: "var(--cody-text-muted)" }}>
              API key
              <input
                // Masked, with no reveal control. The value is written to
                // auth.json by the shell and is cleared here on every path.
                type="password"
                value={apiKey}
                autoComplete="off"
                autoCorrect="off"
                autoCapitalize="off"
                spellCheck={false}
                onChange={(event) => setApiKey(event.target.value)}
                style={{
                  display: "block",
                  width: "100%",
                  marginTop: 4,
                  padding: "6px 10px",
                  borderRadius: 6,
                  border: "1px solid var(--cody-border-strong)",
                  background: "var(--cody-canvas)",
                  color: "var(--cody-text-secondary)",
                  font: "inherit",
                }}
              />
            </label>
          )}
          {provider.requires_key && provider.has_key && (
            <div style={{ color: "var(--cody-success)", fontSize: 12 }}>
              A key already resolves for {provider.name} — no key needed here.
            </div>
          )}

          <details className="model-connection-details">
          <summary>Advanced connection options</summary>
          <label style={{ display: "block", fontSize: 12, color: "var(--cody-text-muted)" }}>
            Model ID
            <input
              value={effectiveId}
              onChange={(event) => {
                setDisplayIdTouched(true);
                setDisplayId(event.target.value);
              }}
              style={{
                display: "block",
                width: "100%",
                marginTop: 4,
                padding: "6px 10px",
                borderRadius: 6,
                border: "1px solid var(--cody-border-strong)",
                background: "var(--cody-canvas)",
                color: "var(--cody-text-secondary)",
                font: "inherit",
              }}
            />
          </label>
          </details>
        </>
      )}

      {error && (
        <div role="status" style={{ color: "var(--cody-danger-soft)", fontSize: 12 }}>
          {error}
        </div>
      )}

      <div style={{ display: "flex", gap: 8 }}>
        <button
          type="submit"
          disabled={busy || !provider}
          style={{ ...BUTTON, border: "1px solid var(--cody-success-strong)", background: "var(--cody-success-strong)", color: "var(--cody-on-accent)" }}
        >
          {busy ? "Adding…" : "Add model"}
        </button>
        <button type="button" style={BUTTON} onClick={onCancel}>
          Cancel
        </button>
      </div>
    </form>
  );
};

export default ModelPicker;
