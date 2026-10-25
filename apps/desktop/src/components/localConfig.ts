/**
 * The webview's call surface for the shell's LOCAL CONFIG commands.
 *
 * `models.toml`, `providers.toml` and `auth.json` are files under the runtime
 * data dir. There is no wire command for any of them, so unlike everything in
 * `transport.ts` these calls work with the daemon stopped — which they must,
 * because configuring a model is what you do before there is a run.
 *
 * No parsing happens here. Every shape below is what
 * `src-tauri/src/models.rs` serialized; the TOML and JSON are read by the same
 * Rust crates the CLI and TUI read them with, so there is exactly one parser
 * per file rather than a second one in TypeScript that drifts.
 *
 * THE SECRET RULE: a key goes one way. `setApiKey` and `addModel` take one;
 * nothing in this module returns one. Presence is reported as `KeyStatus`.
 */
import type React from "react";
import { invoke } from "@tauri-apps/api/core";

/** True only inside the Tauri shell, where these commands exist. */
export function shellAvailable(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/**
 * Credential PRESENCE — never a value.
 *
 * `unknown` is not in the TUI's projection and is the point of this type: the
 * TUI's `/keys` view degrades an unreadable `auth.json` to "no stored keys",
 * which looks exactly like a user who has stored none. A failed read is an
 * outcome and renders as one.
 */
export type KeyStatus =
  | { state: "stored" }
  | { state: "env"; name: string }
  | { state: "missing" }
  | { state: "unknown"; reason: string };

/** One `[[model]]` entry. No readiness field — see `ModelsView.models`. */
export type ModelRow = {
  id: string;
  provider: string;
  base_url: string;
  model: string;
  provider_id: string | null;
  /** `null` is unknown. Render it as unknown; never as 0. */
  context_tokens: number | null;
  key: KeyStatus;
};

export type ModelsView = {
  models: ModelRow[];
  models_path: string;
  /**
   * Whether `models.toml` exists. `false` with an empty list is "nothing
   * configured yet". An unreadable file never reaches here — it throws.
   */
  configured: boolean;
  warnings: string[];
  pinned: string | null;
};

export type DetectedProvider = {
  id: string;
  name: string;
  kind: "local" | "agent" | "hosted";
  state: "ready" | "connected" | "listed" | "credential-found" | "unavailable" | "empty";
  detail: string;
};

export type ModelDiscovery = {
  added: string[];
  default_model: string | null;
  detected: DetectedProvider[];
  warnings: string[];
  available_default_profiles?: string[];
};

export type ProviderRow = {
  id: string;
  name: string;
  protocol: string;
  /** e.g. `api-key: GROQ_API_KEY` — an env var NAME, never a value. */
  auth: string;
  local: boolean;
  requires_key: boolean;
  can_list_models: boolean;
  available: boolean;
  catalog_models: number;
  /** Whether a provider-wide key resolves. A boolean, not the key. */
  has_key: boolean;
  unusable_reason: string | null;
  /** Selecting this row is a trust decision; the confirmation is host chrome. */
  community_consent_required: boolean;
  community_consent_detail: string | null;
};

export type ProvidersView = {
  providers: ProviderRow[];
  providers_path: string;
  warnings: string[];
  /** Why ACP agents are absent from the list. Stated, not silently omitted. */
  acp_unavailable: string;
};

export type CatalogModelRow = {
  id: string;
  name: string | null;
  context_tokens: number | null;
  cost_per_1m_input_usd: number | null;
  cost_per_1m_output_usd: number | null;
};

export type CatalogModelsView = {
  models: CatalogModelRow[];
  warnings: string[];
  /** Why this list is the catalog's rows and not the provider's live ones. */
  live_listing_unavailable: string;
};

/** Daemon-owned verdict. Passive listing never probes provider endpoints. */
export type ModelReadiness = import("@codypendent/protocol").ModelReadiness;
export type ModelReadinessRow = import("@codypendent/protocol").ModelProbe;

/**
 * A surface's outcome sentence, with how loud to say it. Success and failure
 * used to render identically — the same grey 12px line — so "key saved" and
 * "could not save key: auth.json is not writable" were indistinguishable at a
 * glance.
 */
export type Notice = { tone: "ok" | "error"; text: string };

/** Which `auth.json` entry an operation addresses. Carries an id, never a key. */
export type KeyTarget = { kind: "model"; id: string } | { kind: "provider"; id: string };

export type KeyRow = {
  target: KeyTarget;
  label: string;
  detail: string;
  status: KeyStatus;
};

export type KeysView = {
  keys: KeyRow[];
  auth_path: string;
  warnings: string[];
  /** Credential rows this build does not project, and why. */
  unavailable: string;
};

/** The protocol `AgentMode`, serialized with its own tag. */
export type AgentMode =
  | { type: "Ask" }
  | { type: "Explore" }
  | { type: "Plan" }
  | { type: "Build" }
  | { type: "Review" }
  | { type: "Unknown" };

export type ModeCard = {
  mode: AgentMode;
  label: string;
  summary: string;
};

export type RunDefaults = {
  mode: AgentMode;
  /** The pinned model, or `null` for "the daemon chooses". */
  model: string | null;
};

/**
 * The commands themselves. An interface rather than bare functions so a test
 * can substitute one without a Tauri shell — the components take it as a prop.
 */
export interface LocalConfigClient {
  discoverModels?(preferAvailable?: boolean): Promise<ModelDiscovery>;
  listModels(): Promise<ModelsView>;
  setRunModel(model: string | null): Promise<void>;
  addModel(input: {
    displayId: string;
    providerId: string;
    model: string;
    apiKey?: string | null;
    contextTokens?: number | null;
  }): Promise<void>;
  removeModel(modelId: string): Promise<void>;
  listProviders(): Promise<ProvidersView>;
  refreshAgents?(): Promise<ProvidersView>;
  connectAcp?(providerId: string, repository: string, acceptCommunityRisk: boolean): Promise<{ profileId: string; coordinate: string; models: string[] }>;
  listCatalogModels(providerId: string): Promise<CatalogModelsView>;
  listApiKeys(): Promise<KeysView>;
  setApiKey(target: KeyTarget, key: string): Promise<void>;
  removeApiKey(target: KeyTarget): Promise<void>;
  listModes(): Promise<ModeCard[]>;
  runDefaults(): Promise<RunDefaults>;
  setRunMode(mode: AgentMode): Promise<void>;
  /**
   * Readiness for every configured model, without touching provider endpoints.
   * Optional: an older shell has no such command, and the list then shows no
   * badge rather than a guessed one.
   */
  listModelReadiness?(): Promise<ModelReadinessRow[]>;
  /** Readiness for one model; `probe` asks the provider over the network. */
  modelReadiness?(modelId: string, probe: boolean): Promise<ModelReadinessRow>;
}

/**
 * The real client. Every method is one `invoke` of one shell command.
 *
 * `addModel`/`setApiKey` pass the key straight through as an argument and keep
 * no reference to it: the shell writes it to `auth.json` at mode 0600 and the
 * reply is `void`.
 */
export const localConfigClient: LocalConfigClient = {
  discoverModels: (preferAvailable = false) => invoke<ModelDiscovery>("discover_models", { preferAvailable }),
  listModels: () => invoke<ModelsView>("list_models"),
  setRunModel: (model) => invoke<void>("set_run_model", { model }),
  addModel: ({ displayId, providerId, model, apiKey, contextTokens }) =>
    invoke<void>("add_model", {
      displayId,
      providerId,
      model,
      apiKey: apiKey && apiKey.length > 0 ? apiKey : null,
      contextTokens: contextTokens ?? null,
    }),
  removeModel: (modelId) => invoke<void>("remove_model", { modelId }),
  listProviders: () => invoke<ProvidersView>("list_providers"),
  refreshAgents: () => invoke<ProvidersView>("refresh_agents"),
  connectAcp: (providerId, repository, acceptCommunityRisk) => invoke("connect_acp", { providerId, repository, acceptCommunityRisk }),
  listCatalogModels: (providerId) => invoke<CatalogModelsView>("list_catalog_models", { providerId }),
  listApiKeys: () => invoke<KeysView>("list_api_keys"),
  setApiKey: (target, key) => invoke<void>("set_api_key", { target, key }),
  removeApiKey: (target) => invoke<void>("remove_api_key", { target }),
  listModes: () => invoke<ModeCard[]>("list_modes"),
  runDefaults: () => invoke<RunDefaults>("run_defaults"),
  setRunMode: (mode) => invoke<void>("set_run_mode", { mode }),
  listModelReadiness: () => invoke<ModelReadinessRow[]>("list_model_readiness"),
  modelReadiness: (modelId, probe) => invoke<ModelReadinessRow>("model_readiness", { modelId, probe }),
};

/** The glyph, label and colour for a readiness verdict. */
export function describeReadiness(readiness: ModelReadiness): { glyph: string; label: string; color: string } {
  switch (readiness.state) {
    case "ready":
      return { glyph: "●", label: "ready", color: "var(--cody-success)" };
    default:
      return { glyph: "?", label: "unknown", color: "var(--cody-text-muted)" };
    case "unverified":
      return { glyph: "◐", label: "unverified", color: "var(--cody-warning)" };
    case "unavailable":
      return { glyph: "✗", label: "unavailable", color: "var(--cody-danger-soft)" };
  }
}

/** The style a `Notice` renders with. */
export function noticeStyle(notice: Notice): React.CSSProperties {
  return {
    padding: "8px 24px",
    fontSize: 12,
    color: notice.tone === "error" ? "var(--cody-danger-text)" : "var(--cody-text-muted)",
    borderTop: notice.tone === "error" ? "1px solid var(--cody-danger)" : undefined,
    background: notice.tone === "error" ? "var(--cody-danger-bg)" : undefined,
  };
}

/** The message a surface shows when there is no shell to ask. */
export const NO_SHELL =
  "this page is not running inside the Codypendent desktop shell, so it cannot read the local " +
  "configuration files";

/** Error text, without asserting anything about what went wrong. */
export function describeError(error: unknown): string {
  if (typeof error === "string") {
    return error;
  }
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

/** The presence glyph and label for a `KeyStatus`. Never a value. */
export function describeKeyStatus(status: KeyStatus): { glyph: string; label: string; color: string } {
  switch (status.state) {
    case "stored":
      return { glyph: "●", label: "stored", color: "var(--cody-success)" };
    case "env":
      // The NAME of the environment variable, which is not a secret. The value
      // is not read for this projection.
      return { glyph: "◐", label: `from $${status.name}`, color: "var(--cody-link)" };
    case "missing":
      return { glyph: "○", label: "not set", color: "var(--cody-text-muted)" };
    case "unknown":
      return { glyph: "?", label: `unknown — ${status.reason}`, color: "var(--cody-warning)" };
  }
}
