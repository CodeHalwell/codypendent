//! The LOCAL CONFIG half of the desktop: `models.toml`, `providers.toml` and
//! `auth.json` under the runtime data dir.
//!
//! None of this is protocol. There is no `ListModels`, `ListProviders` or
//! `SetApiKey` on the wire — the TUI reads and writes these files directly
//! (`crates/cli/src/tui.rs`), so the shell must too. A webview cannot open a
//! file any more than it can open a socket, which is why the whole surface
//! lives here rather than in TypeScript.
//!
//! Two rules shape every function below.
//!
//! **Nothing is reimplemented that already exists.** `ModelConfig`/`load_models`
//! and `AuthStore` come from `codypendent-runtime`, the provider catalog from
//! `codypendent-providers` — the same types the TUI, the CLI and the daemon
//! parse these files with. Model edits and removal use the runtime's shared
//! `models_file` transaction, including advisory locks, formatting preservation,
//! atomic installation, and credential cleanup.
//!
//! **A secret is never a value that crosses back to the webview.** A key can
//! travel IN, as [`SecretKey`] (whose `Debug` prints `<redacted>`, mirroring
//! `crates/tui/src/action.rs`'s carrier of the same name); what travels OUT is
//! [`KeyStatus`] — stored, or the NAME of an environment variable, or missing.
//! There is no function here that returns key material, and
//! [`KeyTarget`] carries only a non-secret id.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use codypendent_protocol::ModelId;
use codypendent_providers::{AuthMethod, Catalog, Protocol, Provider};
use codypendent_runtime::auth::{AuthStore, Save};
use codypendent_runtime::models::{load_models, provider_auth_id, ModelConfig};
use codypendent_runtime::models_file::update_model_entries;
use serde::{Deserialize, Serialize};

#[path = "model_discovery.rs"]
mod discovery;
pub use discovery::{
    discover_models, reconcile_discovery_default, DetectedProvider, ModelDiscovery,
};

/// The `<data_dir>` every file in this module hangs off, resolved exactly the
/// way `socket_path()` resolves the socket.
fn data_dir() -> anyhow::Result<PathBuf> {
    Ok(crate::runtime_paths::resolve()
        .context("resolving the codypendent data directory")?
        .data_dir)
}

fn models_path(data_dir: &Path) -> PathBuf {
    data_dir.join("models.toml")
}

fn providers_path(data_dir: &Path) -> PathBuf {
    data_dir.join("providers.toml")
}

// ---------------------------------------------------------------------------
// Secret carriers
// ---------------------------------------------------------------------------

/// An API key on its way IN to `auth.json`. Deliberately not `Serialize`: this
/// type can be received from the webview and written to disk, and can never be
/// sent back. Its `Debug` redacts, so a stray `format!("{target:?}")` anywhere
/// downstream cannot leak the value (the discipline of
/// `crates/tui/src/action.rs::SecretKey`).
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct SecretKey(pub String);

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(<redacted>)")
    }
}

/// Which `auth.json` entry an operation addresses. Carries an id, never key
/// material — the same split as `crates/tui/src/action.rs::KeyTarget`.
///
/// Two variants, not the TUI's four: `Tavily`, `Transcription` and `Speech`
/// resolve through `codypendent-integrations` constants and `models.toml`'s
/// `[transcription]`/`[speech]` tables, and hardcoding those load-bearing
/// strings here would silently save keys that read back as absent. They are
/// reported as unavailable rather than guessed at.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KeyTarget {
    /// One configured model, keyed by its `models.toml` display id verbatim.
    Model { id: String },
    /// A provider-wide key, shared by every model added from that provider.
    Provider { id: String },
}

impl KeyTarget {
    /// The `auth.json` map key. A model id is stored verbatim; a provider key
    /// lives under the reserved `provider/<id>` prefix
    /// (`codypendent_runtime::models::provider_auth_id`).
    fn auth_id(&self) -> String {
        match self {
            KeyTarget::Model { id } => id.clone(),
            KeyTarget::Provider { id } => provider_auth_id(id),
        }
    }
}

/// Presence, never value. The projection `crates/tui/src/state.rs::KeyStatus`
/// makes, plus one variant it does not need: the TUI's `/keys` view degrades a
/// corrupt `auth.json` to "no stored keys", which is indistinguishable from a
/// user who has stored none. Here that case is [`KeyStatus::Unknown`], so a
/// failed read never renders as a confident "Missing".
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum KeyStatus {
    /// A key is stored in `auth.json` for this id.
    Stored,
    /// No stored key, but the entry names an environment variable. The NAME is
    /// not a secret; the value is never read for this projection.
    Env { name: String },
    /// Neither a stored key nor a configured environment variable name.
    Missing,
    /// `auth.json` exists but could not be read, so presence is unknown. This
    /// is an outcome, not an absence.
    Unknown { reason: String },
}

// ---------------------------------------------------------------------------
// Reading: models
// ---------------------------------------------------------------------------

/// One `[[model]]` entry as the desktop shows it.
///
/// There is no `readiness` field. The TUI computes one by probing local
/// endpoints and resolving credentials through `ModelRegistry`, which this
/// build does not compile (the shell configures models; the daemon runs them).
/// An unverified model is reported as unverified by omission rather than
/// rendered as ready.
#[derive(Debug, Clone, Serialize)]
pub struct ModelRow {
    pub id: String,
    /// The runtime adapter family (`"openai-compatible"`, `"acp"`).
    pub provider: String,
    pub base_url: String,
    /// The provider-side model name.
    pub model: String,
    /// The provider-catalog id this entry was added from, when known.
    pub provider_id: Option<String>,
    /// The declared context window. `None` is unknown — never rendered as 0.
    pub context_tokens: Option<u64>,
    /// Presence of a credential for this entry. Value never included.
    pub key: KeyStatus,
}

/// What [`list_models`] answers with.
#[derive(Debug, Clone, Serialize)]
pub struct ModelsView {
    pub models: Vec<ModelRow>,
    /// The file these rows came from, so an empty list can name what it read.
    pub models_path: String,
    /// Whether `models.toml` exists at all. `false` + an empty list means "no
    /// models configured yet"; an unreadable file is an `Err`, not this.
    pub configured: bool,
    /// Degradations that did not stop the read (e.g. a `providers.toml` that
    /// did not parse). Surfaced, never swallowed.
    pub warnings: Vec<String>,
    /// The model pinned for the next run, if one is pinned in this shell.
    pub pinned: Option<String>,
}

/// Every configured model, with credential PRESENCE.
///
/// A missing `models.toml` is "read, empty" — the normal state of a fresh
/// install. A `models.toml` that exists but does not parse is an error: the
/// caller renders unavailable, which is not the same thing as an empty list.
pub fn list_models(pinned: Option<&ModelId>) -> anyhow::Result<ModelsView> {
    let data_dir = data_dir()?;
    let path = models_path(&data_dir);
    let configured = path.exists();
    let configs = if configured {
        load_models(&path).with_context(|| format!("reading {}", path.display()))?
    } else {
        Vec::new()
    };

    let mut warnings = Vec::new();
    let auth = load_auth_for_view(&data_dir, &mut warnings);
    // The catalog's documented environment variable NAMES are a real step of
    // the credential precedence, so these rows need it to tell "missing" from
    // "resolvable from the environment".
    let catalog = load_catalog_for_view(&data_dir, &mut warnings);

    let models = configs
        .into_iter()
        .map(|config| {
            let key = model_key_status(&auth, &catalog, &config);
            ModelRow {
                id: config.id.0,
                provider: config.provider,
                base_url: config.base_url,
                model: config.model,
                provider_id: config.provider_id,
                context_tokens: config.context_tokens,
                key,
            }
        })
        .collect();

    Ok(ModelsView {
        models,
        models_path: path.display().to_string(),
        configured,
        warnings,
        pinned: pinned.map(|id| id.0.clone()),
    })
}

/// The key-status projection for one model entry, in the RUNTIME's credential
/// order (`ModelRegistry::api_key_for`): the per-model stored entry, then the
/// PROVIDER-wide stored entry, then the entry's own `api_key_env` NAME, then a
/// documented provider environment variable that is set, then missing.
///
/// The order is the point, not just the steps. `api_key_for` consults both
/// stored entries before it resolves any environment variable, so a projection
/// that puts `api_key_env` second names an unset variable as the source for a
/// model the provider-wide key is actually running — telling the operator to
/// set something that is not what will be used.
///
/// The last two steps are the runtime's own credential precedence
/// (`ModelRegistry::api_key_for`), and this projection went without them while
/// claiming to mirror the TUI's. `add_model_in` deliberately stores the key
/// provider-wide so a second model from the same provider needs no second
/// paste — and that second model then rendered "Missing" in Models and API
/// Keys while running perfectly well.
fn model_key_status(
    auth: &Result<AuthStore, String>,
    catalog: &Catalog,
    config: &ModelConfig,
) -> KeyStatus {
    match auth {
        Err(reason) => KeyStatus::Unknown {
            reason: reason.clone(),
        },
        Ok(auth) => {
            // EITHER stored entry outranks any environment variable, so the
            // two are one condition rather than two arms with one answer.
            let stored = auth.get(&config.id.0).is_some()
                || config
                    .provider_id
                    .as_deref()
                    .and_then(|id| auth.get(&provider_auth_id(id)))
                    .is_some();
            if stored {
                KeyStatus::Stored
            } else if !config.api_key_env.trim().is_empty() {
                KeyStatus::Env {
                    name: config.api_key_env.clone(),
                }
            } else if let Some(name) = config
                .provider_id
                .as_deref()
                .and_then(|id| catalog.get(id))
                .and_then(provider_env_in_use)
            {
                KeyStatus::Env { name }
            } else {
                KeyStatus::Missing
            }
        }
    }
}

/// The provider catalog for a read-only projection. A `providers.toml` that
/// does not parse is a degradation, not a failure: the builtin catalog stands
/// in and the reason is surfaced as a warning rather than failing the read.
fn load_catalog_for_view(data_dir: &Path, warnings: &mut Vec<String>) -> Catalog {
    Catalog::load_with_user_overrides(&providers_path(data_dir)).unwrap_or_else(|error| {
        warnings.push(format!("provider catalog fell back to built-ins ({error})"));
        Catalog::builtin()
    })
}

/// `auth.json` for a read-only projection. A *missing* file is an empty store
/// (`AuthStore::load`'s own contract); a file that exists and does not parse is
/// kept as the error text so every row that depends on it renders
/// [`KeyStatus::Unknown`] instead of a confident "Missing".
fn load_auth_for_view(data_dir: &Path, warnings: &mut Vec<String>) -> Result<AuthStore, String> {
    match AuthStore::load(data_dir) {
        Ok(auth) => Ok(auth),
        Err(error) => {
            let reason = format!(
                "could not read {}: {error}",
                data_dir.join("auth.json").display()
            );
            warnings.push(reason.clone());
            Err(reason)
        }
    }
}

// ---------------------------------------------------------------------------
// Reading: providers
// ---------------------------------------------------------------------------

/// One catalog provider as the desktop shows it. Every derived boolean is the
/// TUI's own gate, ported below rather than re-derived from prose.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderRow {
    pub id: String,
    pub name: String,
    /// Wire protocol label: `openai-chat` | `anthropic` | `gemini-native` |
    /// `acp` | `unknown`.
    pub protocol: String,
    /// Auth label, e.g. `api-key: GROQ_API_KEY` | `none` | `oauth`.
    pub auth: String,
    pub local: bool,
    pub requires_key: bool,
    pub can_list_models: bool,
    /// Whether a model added from this provider is executable by the runtime.
    pub available: bool,
    /// Curated `[[model]]` rows the catalog ships for this provider.
    pub catalog_models: usize,
    /// Whether a provider-wide key resolves (stored, or a documented env var is
    /// set). A boolean — the key itself never crosses this boundary.
    pub has_key: bool,
    /// Why the add-model flow refuses this row, when it does. The TUI's own
    /// refusal text (`crates/tui/src/reduce.rs`).
    pub unusable_reason: Option<String>,
    /// Whether selecting this row must first pass a host-owned trust
    /// confirmation. See [`community_bridge_row`].
    pub community_consent_required: bool,
    /// The risk the confirmation must state, when one is required.
    pub community_consent_detail: Option<String>,
}

/// What [`list_providers`] answers with.
#[derive(Debug, Clone, Serialize)]
pub struct ProvidersView {
    pub providers: Vec<ProviderRow>,
    /// The user overlay file, so the view can name where overrides come from.
    pub providers_path: String,
    pub warnings: Vec<String>,
    /// Discovery limitations, empty when cached/local agents were loaded.
    pub acp_unavailable: String,
}

/// The catalog, layered with the user's `providers.toml`.
///
/// A `providers.toml` that does not parse falls back to the built-ins with a
/// warning — the TUI's behaviour (`load_provider_cards`), and the right one: the
/// built-in catalog is real curated data, not a placeholder.
pub fn list_providers() -> anyhow::Result<ProvidersView> {
    let data_dir = data_dir()?;
    let overlay = providers_path(&data_dir);
    let mut warnings = Vec::new();
    let catalog = match Catalog::load_with_user_overrides(&overlay) {
        Ok(catalog) => catalog,
        Err(error) => {
            warnings.push(format!("provider catalog fell back to built-ins ({error})"));
            Catalog::builtin()
        }
    };

    let mut catalog_model_counts: BTreeMap<String, usize> = BTreeMap::new();
    for model in catalog.models() {
        *catalog_model_counts
            .entry(model.provider_id.clone())
            .or_default() += 1;
    }

    let auth = AuthStore::load(&data_dir).unwrap_or_else(|error| {
        warnings.push(format!(
            "could not read {}: {error}; provider key presence may be incomplete",
            data_dir.join("auth.json").display()
        ));
        AuthStore::default()
    });

    let mut providers: Vec<ProviderRow> = catalog
        .providers()
        .map(|provider| {
            let available = provider_runtime_supported(provider);
            let protocol = protocol_label(provider.protocol).to_owned();
            ProviderRow {
                id: provider.id.clone(),
                name: provider.name.clone(),
                unusable_reason: (!available).then(|| {
                    format!(
                        "{} is catalog-only — its {protocol} runtime adapter is not installed",
                        provider.id
                    )
                }),
                protocol,
                auth: auth_label(provider),
                local: provider.local,
                requires_key: provider_requires_key(provider),
                can_list_models: provider_can_list_models(provider),
                available,
                catalog_models: catalog_model_counts
                    .get(&provider.id)
                    .copied()
                    .unwrap_or_default(),
                has_key: provider_has_resolvable_key(
                    &provider.id,
                    &auth,
                    &provider_key_envs(provider),
                ),
                community_consent_required: false,
                community_consent_detail: None,
            }
        })
        .collect();

    append_acp_rows(&data_dir, &mut providers, &mut warnings);
    deduplicate_agent_rows(&mut providers);

    // Usable first, local endpoints before hosted, then by name — the TUI's
    // ordering, so the same provider is in the same place in both clients.
    providers.sort_by(|a, b| {
        b.available
            .cmp(&a.available)
            .then_with(|| b.local.cmp(&a.local))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(ProvidersView {
        providers,
        providers_path: overlay.display().to_string(),
        warnings,
        acp_unavailable: String::new(),
    })
}

fn deduplicate_agent_rows(providers: &mut Vec<ProviderRow>) {
    let available: BTreeSet<_> = providers
        .iter()
        .filter(|row| row.protocol == "acp" && row.available)
        .map(|row| row.id.clone())
        .collect();
    providers.retain(|row| {
        let canonical = if row.id == "github-copilot-cli" {
            "copilot".to_string()
        } else {
            codypendent_integrations::acp_registry::canonical_agent_id(&row.id)
        };
        row.protocol != "acp" || row.id == canonical || !available.contains(&canonical)
    });
}

/// The one provider id whose selection is a trust decision rather than a
/// configuration choice.
const COMMUNITY_ACP_BRIDGE_ID: &str = "antigravity-community";

/// The host-owned row for the community Antigravity bridge.
///
/// Its host-owned consent requirement cannot be overwritten by catalog data.
fn community_bridge_row() -> ProviderRow {
    ProviderRow {
        id: COMMUNITY_ACP_BRIDGE_ID.to_string(),
        name: "Google Antigravity (community bridge)".to_string(),
        protocol: "acp".to_string(),
        auth: "acp: verified install · third-party ToS risk".to_string(),
        local: true,
        requires_key: false,
        can_list_models: false,
        available: false,
        catalog_models: 0,
        has_key: false,
        unusable_reason: Some(
            "No verified community bridge distribution is available for this platform.".to_string(),
        ),
        community_consent_required: true,
        community_consent_detail: Some(
            "This legacy community bridge is not published, reviewed or supported by Google, \
             and using it may breach the terms of the account it drives."
                .to_string(),
        ),
    }
}

/// Read cached metadata and local executables only. Refresh is an explicit action.
fn append_acp_rows(data_dir: &Path, providers: &mut Vec<ProviderRow>, warnings: &mut Vec<String>) {
    use codypendent_integrations::acp_registry::{self, AcpRegistryStore};
    let store = AcpRegistryStore::new(data_dir);
    let registry = store.load_cached();
    if let Err(error) = &registry {
        warnings.push(format!(
            "Refresh the agent catalog to discover official ACP adapters ({error})"
        ));
    }
    let mut agents = registry.map(|registry| registry.agents).unwrap_or_default();
    if let Some(community) = acp_registry::community_acp_agent(COMMUNITY_ACP_BRIDGE_ID) {
        agents.retain(|agent| agent.id != COMMUNITY_ACP_BRIDGE_ID);
        agents.push(community);
    }
    for agent in agents {
        let ready = store.launch_spec(&agent.id).is_ok();
        let installable = agent
            .distribution
            .binary
            .get(acp_registry::current_platform())
            .is_some_and(|binary| binary.sha256.is_some());
        let available = ready || installable || store.can_prepare_launch(&agent.id);
        let mut row = ProviderRow {
            id: agent.id.clone(), name: agent.name, protocol: "acp".into(),
            auth: format!("agent-managed sign-in · {}", agent.version), local: false,
            requires_key: false, can_list_models: ready, available, catalog_models: 0,
            has_key: false, unusable_reason: (!available).then(|| "Install this agent's package runner or use a verified distribution for this platform".into()),
            community_consent_required: false, community_consent_detail: None,
        };
        if row.id == COMMUNITY_ACP_BRIDGE_ID {
            let trusted = community_bridge_row();
            row.community_consent_required = true;
            row.community_consent_detail = trusted.community_consent_detail;
        }
        providers.retain(|other| other.id != row.id);
        providers.push(row);
    }
    for spec in store.installed_agent_specs() {
        let mut row = ProviderRow {
            id: spec.registry_id,
            name: spec.name,
            protocol: "acp".into(),
            auth: format!("local agent · {}", spec.version),
            local: true,
            requires_key: false,
            can_list_models: true,
            available: true,
            catalog_models: 0,
            has_key: false,
            unusable_reason: None,
            community_consent_required: false,
            community_consent_detail: None,
        };
        if row.id == COMMUNITY_ACP_BRIDGE_ID {
            row.community_consent_required = true;
            row.community_consent_detail = community_bridge_row().community_consent_detail;
        }
        providers.retain(|other| other.id != row.id);
        providers.push(row);
    }
    for &(id, name) in acp_registry::supported_agent_names() {
        if providers
            .iter()
            .any(|row| row.id == id && row.protocol == "acp")
        {
            continue;
        }
        providers.push(ProviderRow {
            id: id.into(),
            name: name.into(),
            protocol: "acp".into(),
            auth: "agent-managed sign-in".into(),
            local: true,
            requires_key: false,
            can_list_models: false,
            available: false,
            catalog_models: 0,
            has_key: false,
            unusable_reason: Some(format!(
                "Install {name} or its ACP adapter; existing sign-ins are reused automatically"
            )),
            community_consent_required: false,
            community_consent_detail: None,
        });
    }
    // A user catalog must not bypass the community gate, even on an unsupported platform.
    for row in providers
        .iter_mut()
        .filter(|row| row.id == COMMUNITY_ACP_BRIDGE_ID)
    {
        row.community_consent_required = true;
        row.community_consent_detail = community_bridge_row().community_consent_detail;
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectedAcp {
    pub profile_id: String,
    pub coordinate: String,
    pub models: Vec<String>,
}

pub async fn refresh_agents() -> anyhow::Result<ProvidersView> {
    codypendent_integrations::acp_registry::AcpRegistryStore::new(data_dir()?)
        .refresh()
        .await?;
    list_providers()
}

pub async fn connect_acp(
    provider_id: &str,
    repository: &Path,
    accept_community_risk: bool,
) -> anyhow::Result<ConnectedAcp> {
    let data_dir = data_dir()?;
    let probe = codypendent_runtime::acp_setup::probe_agent(
        &data_dir,
        provider_id,
        repository,
        accept_community_risk,
    )
    .await?;
    let profile_id = format!("acp/{}", probe.registry_id);
    let coordinate = probe.coordinate(None);
    let mut profiles = vec![(profile_id.clone(), coordinate.clone())];
    profiles.extend(probe.models.iter().map(|model| {
        (
            format!("{profile_id}#{model}"),
            probe.coordinate(Some(model)),
        )
    }));
    codypendent_runtime::acp_setup::replace_profile_family(&data_dir, &profile_id, &profiles)?;
    Ok(ConnectedAcp {
        profile_id,
        coordinate,
        models: probe.models,
    })
}

/// `crates/cli/src/tui.rs::protocol_label`.
fn protocol_label(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenAiChat => "openai-chat",
        Protocol::Anthropic => "anthropic",
        Protocol::GeminiNative => "gemini-native",
        Protocol::Acp => "acp",
        // `Protocol` is `#[non_exhaustive]`: a future variant this build does
        // not understand still renders, rather than failing to compile.
        _ => "unknown",
    }
}

/// The auth label `load_provider_cards` shows. Names environment VARIABLES,
/// never values.
fn auth_label(provider: &Provider) -> String {
    match provider.auth.first() {
        None | Some(AuthMethod::None) => "none".to_string(),
        Some(AuthMethod::ApiKey { env, .. }) => {
            format!("api-key: {}", env.first().map(String::as_str).unwrap_or(""))
        }
        Some(AuthMethod::Acp { command, .. }) => format!("acp: {command}"),
        Some(AuthMethod::CloudIam { variant, .. }) => format!("cloud-iam: {variant}"),
        Some(AuthMethod::OAuth { .. }) => "oauth".to_string(),
        Some(_) => "unknown".to_string(),
    }
}

/// The documented env var NAMES for a provider's API-key auth method, if it has
/// one. Names only.
fn provider_key_envs(provider: &Provider) -> Vec<String> {
    provider
        .auth
        .iter()
        .find_map(|method| match method {
            AuthMethod::ApiKey { env, .. } => Some(env.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The documented env var for this provider that is actually SET, if any.
///
/// `crates/cli/src/tui.rs::provider_env_in_use`. A name alone proves nothing —
/// the row says "Env" only when the variable carries a value.
fn provider_env_in_use(provider: &Provider) -> Option<String> {
    provider_key_envs(provider)
        .into_iter()
        .find(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
}

/// `crates/cli/src/tui.rs::provider_requires_key`.
fn provider_requires_key(provider: &Provider) -> bool {
    matches!(provider.auth.first(), Some(AuthMethod::ApiKey { .. }))
}

/// `crates/cli/src/tui.rs::provider_endpoint_usable`.
fn provider_endpoint_usable(provider: &Provider) -> bool {
    provider
        .base_url
        .as_deref()
        .is_some_and(|url| !url.trim().is_empty())
        && matches!(
            provider.auth.first(),
            Some(AuthMethod::ApiKey { .. } | AuthMethod::None) | None
        )
}

/// `crates/cli/src/tui.rs::provider_can_list_models`.
fn provider_can_list_models(provider: &Provider) -> bool {
    matches!(provider.protocol, Protocol::OpenAiChat) && provider_endpoint_usable(provider)
}

/// `crates/cli/src/tui.rs::provider_runtime_supported` — the gate that decides
/// whether the add-model flow may write an entry for this provider at all.
fn provider_runtime_supported(provider: &Provider) -> bool {
    matches!(
        provider.protocol,
        Protocol::OpenAiChat | Protocol::Anthropic | Protocol::GeminiNative
    ) && provider_endpoint_usable(provider)
}

/// `crates/cli/src/tui.rs::resolve_provider_api_key`, reduced to the boolean
/// its caller actually needs. Precedence: `auth.json[provider/<id>]`, then the
/// first non-blank documented environment value. Blank and whitespace-only
/// values are absent, never a valid key. The value is dropped here and never
/// returned.
fn provider_has_resolvable_key(provider_id: &str, auth: &AuthStore, env_names: &[String]) -> bool {
    if auth
        .get(&provider_auth_id(provider_id))
        .is_some_and(|key| !key.trim().is_empty())
    {
        return true;
    }
    env_names
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
}

// ---------------------------------------------------------------------------
// Reading: the catalog's curated models for one provider
// ---------------------------------------------------------------------------

/// One curated `[[model]]` row from the provider catalog: what the add-model
/// flow can offer with no network at all.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogModelRow {
    pub id: String,
    pub name: Option<String>,
    /// `None` is unknown. Never rendered as 0.
    pub context_tokens: Option<u64>,
    pub cost_per_1m_input_usd: Option<f64>,
    pub cost_per_1m_output_usd: Option<f64>,
}

/// What [`list_catalog_models`] answers with.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogModelsView {
    pub models: Vec<CatalogModelRow>,
    pub warnings: Vec<String>,
    /// Why a live `/models` list is not offered. The TUI additionally GETs
    /// `{base_url}/models` with the provider key
    /// (`crates/cli/src/tui.rs::query_provider_models`); this shell does not,
    /// so a provider whose curated rows are empty offers the free-text path
    /// rather than a list that looks exhaustive and is not.
    pub live_listing_unavailable: String,
}

const LIVE_LISTING_UNAVAILABLE: &str = "These are the catalog's curated rows, not the provider's \
live /models response. This shell does not query the provider; a model the catalog does not list \
can still be added by typing its provider-side name.";

/// The curated catalog rows for `provider_id`, in catalog order.
pub fn list_catalog_models(provider_id: &str) -> anyhow::Result<CatalogModelsView> {
    let data_dir = data_dir()?;
    let mut warnings = Vec::new();
    let catalog = load_catalog_for_view(&data_dir, &mut warnings);
    if catalog.get(provider_id).is_none() {
        bail!("provider `{provider_id}` is not in the catalog");
    }
    let models = catalog
        .models()
        .filter(|model| model.provider_id == provider_id)
        .map(|model| CatalogModelRow {
            id: model.id.clone(),
            name: model.name.clone(),
            context_tokens: model.context_tokens,
            cost_per_1m_input_usd: model.cost_per_1m_input_usd,
            cost_per_1m_output_usd: model.cost_per_1m_output_usd,
        })
        .collect();
    Ok(CatalogModelsView {
        models,
        warnings,
        live_listing_unavailable: LIVE_LISTING_UNAVAILABLE.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Reading: key statuses
// ---------------------------------------------------------------------------

/// One row of the API-key surface. Presence only.
#[derive(Debug, Clone, Serialize)]
pub struct KeyRow {
    pub target: KeyTarget,
    pub label: String,
    /// Non-secret context: which endpoint, or which provider.
    pub detail: String,
    pub status: KeyStatus,
}

/// What [`key_statuses`] answers with.
#[derive(Debug, Clone, Serialize)]
pub struct KeysView {
    pub keys: Vec<KeyRow>,
    pub auth_path: String,
    pub warnings: Vec<String>,
    /// Credential rows this build does not project, and why. The TUI's `/keys`
    /// also lists the Tavily search key and the `[transcription]`/`[speech]`
    /// voice keys; their `auth.json` ids are load-bearing constants owned by
    /// `codypendent-integrations` and `codypendent-runtime`'s audio clients, so
    /// they are declared unavailable rather than guessed.
    pub unavailable: String,
}

const KEYS_UNAVAILABLE: &str = "Saved keys stay on this computer. Existing agent sign-ins are \
managed by their apps.";

/// The host part of a `base_url`, for display only — the projection
/// `crates/cli/src/tui.rs::endpoint_host` makes. A string trim rather than a URL
/// parse: this feeds a label, and a `base_url` it cannot make sense of should
/// render as itself rather than vanish.
fn endpoint_host(base_url: &str) -> &str {
    let rest = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest)
        .trim_start_matches('/');
    let host = rest.split('/').next().unwrap_or(rest);
    if host.is_empty() {
        base_url
    } else {
        host
    }
}

/// Which credentials are set — one row per configured model, plus one per
/// provider that either backs a configured model or already holds a
/// provider-wide key.
///
/// Never returns a key. A corrupt `auth.json` yields [`KeyStatus::Unknown`]
/// rows rather than a page of confident "Missing".
pub fn key_statuses() -> anyhow::Result<KeysView> {
    key_statuses_in(&data_dir()?)
}

fn key_statuses_in(data_dir: &Path) -> anyhow::Result<KeysView> {
    let path = models_path(data_dir);
    let configs = if path.exists() {
        load_models(&path).with_context(|| format!("reading {}", path.display()))?
    } else {
        Vec::new()
    };

    let mut warnings = Vec::new();
    let auth = load_auth_for_view(data_dir, &mut warnings);
    let catalog = load_catalog_for_view(data_dir, &mut warnings);

    let mut keys: Vec<KeyRow> = configs
        .iter()
        .filter(|config| {
            config.provider != "acp"
                && (config
                    .provider_id
                    .as_deref()
                    .and_then(|id| catalog.get(id))
                    .is_none_or(provider_requires_key)
                    || auth
                        .as_ref()
                        .is_ok_and(|auth| auth.get(&config.id.0).is_some()))
        })
        .map(|config| KeyRow {
            target: KeyTarget::Model {
                id: config.id.0.clone(),
            },
            label: config.id.0.clone(),
            detail: format!("{} · {}", config.model, endpoint_host(&config.base_url)),
            status: model_key_status(&auth, &catalog, config),
        })
        .collect();

    // Provider-wide rows: every provider a configured model was added from,
    // plus every provider that already holds a `provider/<id>` entry, so a key
    // stored for a provider whose models were all removed is still visible and
    // still removable. Never every catalog provider — 39 Missing rows would
    // bury the ones that mean something.
    let mut provider_ids: Vec<String> = configs
        .iter()
        .filter_map(|config| config.provider_id.clone())
        .filter(|id| catalog.get(id).is_none_or(provider_requires_key))
        .collect();
    if let Ok(auth) = &auth {
        // From the STORE, not the catalog. A custom provider deleted from
        // `providers.toml` leaves its provider-wide key behind, and taking the
        // ids from the catalog made that secret invisible on the page whose job
        // is to remove it — present on disk, unreachable from the UI.
        for id in auth.ids() {
            if let Some(provider_id) = id.strip_prefix("provider/") {
                provider_ids.push(provider_id.to_owned());
            }
        }
    }
    provider_ids.sort();
    provider_ids.dedup();

    for provider_id in provider_ids {
        let envs = catalog
            .get(&provider_id)
            .map(provider_key_envs)
            .unwrap_or_default();
        let name = catalog
            .get(&provider_id)
            .map_or_else(|| provider_id.clone(), |provider| provider.name.clone());
        let status = match &auth {
            Err(reason) => KeyStatus::Unknown {
                reason: reason.clone(),
            },
            Ok(auth) => {
                if auth.get(&provider_auth_id(&provider_id)).is_some() {
                    KeyStatus::Stored
                } else if let Some(name) = envs
                    .iter()
                    .find(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
                {
                    KeyStatus::Env { name: name.clone() }
                } else {
                    KeyStatus::Missing
                }
            }
        };
        keys.push(KeyRow {
            target: KeyTarget::Provider {
                id: provider_id.clone(),
            },
            label: format!("{name} (provider-wide)"),
            detail: if envs.is_empty() {
                format!("provider/{provider_id}")
            } else {
                format!("provider/{provider_id} · {}", envs.join(", "))
            },
            status,
        });
    }

    Ok(KeysView {
        keys,
        auth_path: data_dir.join("auth.json").display().to_string(),
        warnings,
        unavailable: KEYS_UNAVAILABLE.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Writing: auth.json
// ---------------------------------------------------------------------------

/// Store one API key, or remove one. A port of
/// `crates/cli/src/tui.rs::write_api_key`, with its two guards intact:
///
/// * a blank or whitespace-only key is REFUSED, because `set(id, "")` would
///   silently shadow a valid `api_key_env` and look like it worked;
/// * `AuthStore::load` runs before any write, so a hand-corrupted `auth.json`
///   aborts with a legible error instead of being replaced by a fresh store
///   holding one entry.
///
/// Removing an absent entry skips the save entirely — nothing changed, and no
/// empty `auth.json` is created for a store that never existed.
pub fn write_api_key(target: &KeyTarget, key: Option<&SecretKey>) -> anyhow::Result<()> {
    let data_dir = data_dir()?;
    let id = target.auth_id();
    // Under the shared hold: the CLI and this shell both write `auth.json`, and
    // each write renames a whole map back, so an unserialized pair silently
    // dropped one of the two credentials.
    codypendent_runtime::auth::update(&data_dir, |auth| -> anyhow::Result<_> {
        match key {
            Some(key) => {
                let key = key.0.trim();
                if key.is_empty() {
                    bail!("key must not be blank");
                }
                auth.set(id, key);
                Ok((Save::Yes, ()))
            }
            // Removing an absent entry writes nothing, so a store that never
            // existed is not created empty.
            None if auth.remove(&id) => Ok((Save::Yes, ())),
            None => Ok((Save::No, ())),
        }
    })
    .with_context(|| format!("writing {}", data_dir.join("auth.json").display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Writing: models.toml (add)
// ---------------------------------------------------------------------------

/// A provider `base_url` as it should be persisted: trailing slashes trimmed.
/// `crates/cli/src/tui.rs::normalize_base_url` — the catalog stores a few with
/// one (`…/v1/`) and the chat client joins `{base}/chat/completions`, so the raw
/// value would produce a double slash on every request.
fn normalize_base_url(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

/// Add (or update in place) one `[[model]]` entry, and optionally store its key.
///
/// A port of `crates/cli/src/tui.rs::write_add_model` minus its ACP arm (this
/// build cannot launch ACP agents). Every refusal is carried:
///
/// * a blank `display_id` writes nothing to EITHER file;
/// * a blank `api_key` is treated exactly as `None` — the `auth.json` write is
///   skipped rather than storing an empty string that would shadow a valid
///   `api_key_env`;
/// * when a key IS present, `auth.json` is loaded BEFORE `models.toml` is
///   written, so a corrupt key store aborts the whole add rather than leaving a
///   keyless model entry behind;
/// * a provider absent from the catalog, or one whose runtime adapter is not
///   installed, is a hard error — never a written entry that can only fail at
///   run time;
/// * a duplicate `display_id` UPDATES in place; it never appends a second row.
pub fn add_model(
    display_id: &str,
    provider_id: &str,
    model: &str,
    api_key: Option<&SecretKey>,
    context_tokens: Option<u64>,
) -> anyhow::Result<()> {
    let data_dir = data_dir()?;
    add_model_in(
        &data_dir,
        display_id,
        provider_id,
        model,
        api_key,
        context_tokens,
    )
}

/// [`add_model`] against an explicit data directory, so the write ordering it
/// depends on can be tested without the process-wide path resolution.
fn add_model_in(
    data_dir: &Path,
    display_id: &str,
    provider_id: &str,
    model: &str,
    api_key: Option<&SecretKey>,
    context_tokens: Option<u64>,
) -> anyhow::Result<()> {
    if display_id.trim().is_empty() {
        bail!("model id must not be blank");
    }
    // Not in the reference, which reaches this function only from a picker that
    // cannot produce a blank model. Reached from a webview, it can — and an
    // entry with an empty provider-side model name is unrunnable, so it is
    // refused here rather than written.
    if model.trim().is_empty() {
        bail!("provider-side model name must not be blank");
    }
    let display_id = display_id.trim();
    let model = model.trim();

    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating the data dir {}", data_dir.display()))?;

    // A blank/whitespace-only key is treated exactly like `None`, filtered up
    // front so the load-order guard below and the final write agree on whether
    // a key is really present.
    let key = api_key.filter(|key| !key.0.trim().is_empty());

    // All-or-nothing: when a key is present, load auth.json NOW, before
    // models.toml is written, so a corrupt store surfaces here instead of
    // leaving a keyless models.toml entry behind while the key silently fails
    // to save. A keyless add loads no auth.json at all. The matching half —
    // saving the key before models.toml, not after — is below.
    //
    // A guard, never the basis for the write: the save below re-reads the store
    // under the shared hold, because this snapshot can be stale by the time it
    // is written and saving it back would discard another writer's credential.
    if key.is_some() {
        AuthStore::load(data_dir)
            .with_context(|| format!("reading {}", data_dir.join("auth.json").display()))?;
    }

    let catalog = Catalog::load_with_user_overrides(&providers_path(data_dir))
        .unwrap_or_else(|_| Catalog::builtin());
    let provider = catalog
        .get(provider_id)
        .ok_or_else(|| anyhow!("provider `{provider_id}` is not in the catalog"))?;
    if provider_id == COMMUNITY_ACP_BRIDGE_ID {
        bail!(
            "`{COMMUNITY_ACP_BRIDGE_ID}` is a community ACP bridge; this shell cannot install or \
             run ACP agents"
        );
    }
    if !provider_runtime_supported(provider) {
        bail!(
            "provider `{provider_id}` uses {} and is not executable by this build",
            protocol_label(provider.protocol)
        );
    }
    let base_url = normalize_base_url(provider.base_url.as_deref().ok_or_else(|| {
        anyhow!("provider `{provider_id}` has no base URL and cannot be configured")
    })?);

    // A catalog row for this exact model fills in the context window when the
    // caller did not already know it.
    let context_tokens = context_tokens.or_else(|| {
        catalog
            .model(provider_id, model)
            .and_then(|row| row.context_tokens)
    });

    let config = ModelConfig {
        id: ModelId(display_id.to_string()),
        provider: "openai-compatible".to_string(),
        base_url,
        model: model.to_string(),
        // The key, if any, goes to auth.json — never into models.toml, which
        // holds env var NAMES only.
        api_key_env: String::new(),
        provider_id: Some(provider_id.to_string()),
        context_tokens,
    };
    // The key is saved BEFORE models.toml, because the two possible partial
    // outcomes are not equally bad. Loading auth.json early already caught a
    // corrupt store, but `save` can still fail on its own — a full disk, a
    // permission change — and doing it second left the model listed with no key
    // behind it: an entry the user sees as configured and that fails at the
    // first request, with the add having reported success.
    //
    // Saved first, the only partial outcome is a key in auth.json whose model
    // entry was never written. That is inert: nothing reads a key for a model
    // that does not exist, and a retry overwrites it.
    if let Some(key) = key {
        let key = key.0.trim();
        codypendent_runtime::auth::update(data_dir, |auth| -> anyhow::Result<_> {
            auth.set(display_id, key);
            // Also store it provider-wide, so adding a second model from the
            // same provider needs no second paste of the same key. The runtime
            // reads this entry after the per-model one.
            auth.set(provider_auth_id(provider_id), key);
            Ok((Save::Yes, ()))
        })
        .with_context(|| format!("writing {}", data_dir.join("auth.json").display()))?;
    }

    update_model_entries(&models_path(data_dir), |configs| {
        configs.retain(|existing| existing.id.0 != display_id);
        configs.push(config);
        Ok(())
    })?;
    Ok(())
}

/// Serialize one read-modify-write of the shared model list under an advisory
/// lock, preserving every other table.
///
/// Remove through the same configuration transaction as CLI and TUI.
pub fn remove_model(model_id: &str) -> anyhow::Result<()> {
    codypendent_runtime::models_file::remove_model(&data_dir()?, model_id)
}

/// Whether `model_id` names an entry that is actually in `models.toml`.
///
/// The gate under pinning a model for the next run: a pin that names nothing
/// configured would make the daemon refuse the run later, with the refusal
/// attributed to the run rather than to the pin.
pub fn model_is_configured(model_id: &ModelId) -> anyhow::Result<bool> {
    let data_dir = data_dir()?;
    let path = models_path(&data_dir);
    if !path.exists() {
        return Ok(false);
    }
    let configs = load_models(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(configs.iter().any(|config| config.id == *model_id))
}

// ---------------------------------------------------------------------------
// Mode
// ---------------------------------------------------------------------------

/// One selectable agent mode.
///
/// `crates/tui/src/state.rs::MODE_CARDS` is `pub(crate)` and cannot be
/// imported, so the five cards are re-declared here with the labels and
/// summaries copied from it verbatim. `AgentMode` itself is the protocol enum —
/// only the presentation text is duplicated.
#[derive(Debug, Clone, Serialize)]
pub struct ModeCard {
    /// The protocol `AgentMode`, serialized as `{ "type": "Build" }`.
    pub mode: codypendent_protocol::AgentMode,
    pub label: &'static str,
    pub summary: &'static str,
}

/// The modes the picker offers, in the TUI's presentation order.
pub fn mode_cards() -> Vec<ModeCard> {
    use codypendent_protocol::AgentMode;
    vec![
        ModeCard {
            mode: AgentMode::Ask,
            label: "Ask",
            summary: "read-only Q&A — no writes, commands, or network",
        },
        ModeCard {
            mode: AgentMode::Explore,
            label: "Explore",
            summary: "read-only investigation — no writes, commands, or network",
        },
        ModeCard {
            mode: AgentMode::Plan,
            label: "Plan",
            summary: "investigate read-only, then finish with a numbered implementation plan",
        },
        ModeCard {
            mode: AgentMode::Build,
            label: "Build",
            summary: "full worktree access — writes, commands, and network (the default)",
        },
        ModeCard {
            mode: AgentMode::Review,
            label: "Review",
            summary: "read-only verification with commands — no writes or network",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_and_keyless_local_profiles_do_not_ask_for_api_keys() {
        let directory = tempfile::tempdir().unwrap();
        let path = models_path(directory.path());
        std::fs::write(
            &path,
            r#"
[[model]]
id = "acp/codex"
provider = "acp"
model = "codex-acp@1.0.0"
[[model]]
id = "local"
provider = "openai-compatible"
provider_id = "ollama"
base_url = "http://localhost:11434/v1"
model = "coder"
[[model]]
id = "hosted"
provider = "openai-compatible"
provider_id = "groq"
base_url = "https://api.groq.com/openai/v1"
model = "chat"
"#,
        )
        .unwrap();
        let keys = key_statuses_in(directory.path()).unwrap();
        assert_eq!(keys.keys.len(), 2);
        assert!(keys
            .keys
            .iter()
            .all(|row| !row.label.contains("codex") && row.label != "local"));
        assert!(keys
            .keys
            .iter()
            .any(|row| matches!(&row.target, KeyTarget::Model { id } if id == "hosted")));
    }

    #[test]
    fn installed_agents_replace_legacy_duplicate_provider_rows() {
        let mut rows: Vec<_> = [
            "claude-code",
            "claude-acp",
            "codex",
            "codex-acp",
            "gemini-cli",
            "gemini",
            "github-copilot-cli",
            "copilot",
            "antigravity-acp",
        ]
        .into_iter()
        .map(|id| {
            let mut row = community_bridge_row();
            row.id = id.to_owned();
            row.available = true;
            row
        })
        .collect();
        deduplicate_agent_rows(&mut rows);
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec![
                "claude-acp",
                "codex-acp",
                "gemini",
                "copilot",
                "antigravity-acp"
            ]
        );
        assert!(rows.last().unwrap().community_consent_required);
    }

    /// A key that cannot be saved must not leave a model behind that looks
    /// configured.
    ///
    /// Loading `auth.json` before any write already caught a corrupt store, but
    /// the save itself can fail on its own — a full disk, a permission change —
    /// and it used to run AFTER `models.toml` was written. That left the model
    /// listed with no key behind it: an entry the picker shows as ready, which
    /// fails at the first request, from an add that reported success.
    ///
    /// Forced here by pre-creating the temp file `AuthStore::save` renames from
    /// as a directory: `load` reads `auth.json` (absent, so it succeeds) while
    /// `save` cannot open its temp path. Nothing else in the directory is
    /// unwritable, so a `models.toml` left behind is the ordering bug and not a
    /// side effect of the fixture.
    #[test]
    fn a_model_is_not_written_when_its_key_cannot_be_saved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tmp_path = dir
            .path()
            .join(format!(".auth-{}.json.tmp", std::process::id()));
        std::fs::create_dir(&tmp_path).expect("occupy the save temp path");

        let error = add_model_in(
            dir.path(),
            "groq/llama-3.1-8b-instant",
            "groq",
            "llama-3.1-8b-instant",
            Some(&SecretKey("sk-test".to_string())),
            None,
        )
        .expect_err("saving the key is impossible, so the add must fail");
        assert!(
            !format!("{error:?}").contains("sk-test"),
            "a failed write must not echo the key: {error:?}"
        );

        assert!(
            !models_path(dir.path()).exists(),
            "models.toml must not hold a model whose key was never saved"
        );
    }

    /// The same add, with the key savable, writes both halves.
    ///
    /// The companion to the test above: it proves the failure there came from
    /// the blocked save and not from the fixture refusing every add.
    #[test]
    fn a_model_and_its_key_are_both_written_when_the_save_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");

        add_model_in(
            dir.path(),
            "groq/llama-3.1-8b-instant",
            "groq",
            "llama-3.1-8b-instant",
            Some(&SecretKey("sk-test".to_string())),
            None,
        )
        .expect("add");

        let written = std::fs::read_to_string(models_path(dir.path())).expect("models.toml");
        assert!(
            written.contains("groq/llama-3.1-8b-instant"),
            "the model must be listed: {written}"
        );
        assert!(
            !written.contains("sk-test"),
            "models.toml must never hold key material: {written}"
        );
        let auth = AuthStore::load(dir.path()).expect("auth.json");
        assert_eq!(auth.get("groq/llama-3.1-8b-instant"), Some("sk-test"));
    }

    /// A provider-wide stored key outranks the entry's own `api_key_env`, as
    /// it does in the runtime.
    ///
    /// `ModelRegistry::api_key_for` consults BOTH stored entries before it
    /// resolves any environment variable. Reporting `api_key_env` first named
    /// an unset variable as the source for a model the stored key was actually
    /// running — pointing the operator at something that is not what will be
    /// used, and letting onboarding call the credentials unsatisfied.
    #[test]
    fn a_provider_wide_key_outranks_an_unset_api_key_env() {
        let auth = {
            let mut store = AuthStore::default();
            store.set(provider_auth_id("groq"), "sk-provider-wide");
            Ok(store)
        };
        let config = ModelConfig {
            id: ModelId("groq/llama".to_string()),
            provider: "openai-compatible".to_string(),
            base_url: "https://api.groq.com/openai/v1".to_string(),
            model: "llama-3.1-8b-instant".to_string(),
            // Named, and deliberately not set in this process.
            api_key_env: "CODYPENDENT_TEST_UNSET_KEY_ENV".to_string(),
            provider_id: Some("groq".to_string()),
            context_tokens: None,
        };
        assert!(
            std::env::var("CODYPENDENT_TEST_UNSET_KEY_ENV").is_err(),
            "the fixture needs this variable unset"
        );

        let status = model_key_status(&auth, &Catalog::builtin(), &config);
        assert!(
            matches!(status, KeyStatus::Stored),
            "the stored provider-wide key is what runs this model, but the row said {status:?}"
        );
    }

    /// A second model from a provider whose key is stored PROVIDER-wide reads
    /// as stored, not missing.
    ///
    /// `add_model_in` writes both a per-model and a provider-wide entry, so the
    /// next model from that provider needs no second paste of the same key.
    /// The projection only looked at the per-model entry, so that second model
    /// rendered "Missing" in Models and API Keys the moment it was added —
    /// contradicting a Test that would have run it.
    #[test]
    fn a_provider_wide_key_is_not_reported_missing() {
        let dir = tempfile::tempdir().expect("tempdir");

        add_model_in(
            dir.path(),
            "groq/first",
            "groq",
            "llama-3.1-8b-instant",
            Some(&SecretKey("sk-test".to_string())),
            None,
        )
        .expect("add the first model, with the key");
        // The second add carries NO key: the provider-wide entry is the point.
        add_model_in(
            dir.path(),
            "groq/second",
            "groq",
            "llama-3.3-70b-versatile",
            None,
            None,
        )
        .expect("add the second model, without one");

        let auth = load_auth_for_view(dir.path(), &mut Vec::new());
        let catalog = Catalog::builtin();
        let configs = load_models(&models_path(dir.path())).expect("models.toml");
        let second = configs
            .iter()
            .find(|config| config.id.0 == "groq/second")
            .expect("the second model is listed");

        assert!(
            auth.as_ref()
                .expect("auth.json")
                .get("groq/second")
                .is_none(),
            "the fixture is only meaningful while the second model has no per-model entry"
        );
        let status = model_key_status(&auth, &catalog, second);
        assert!(
            matches!(status, KeyStatus::Stored),
            "the provider-wide key resolves this model, but the row said {status:?}"
        );
    }

    /// A model add must not discard a credential another writer stored while
    /// it was working.
    ///
    /// `add_model_in` loads `auth.json` early to fail fast on a corrupt store,
    /// and used to save that same snapshot back. Every save renames a whole map
    /// back, so a key written in between — by `codypendent models`, or by this
    /// shell's own API Keys screen — was silently dropped while both writers
    /// reported success. The save now re-reads under the shared hold.
    ///
    /// Repetition, not an injected sleep, widens the window: nothing here can
    /// reach inside `add_model_in` to pause it mid-transaction.
    #[test]
    fn a_concurrent_key_write_survives_a_model_add() {
        const ROUNDS: usize = 20;
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_path_buf();

        std::thread::scope(|scope| {
            scope.spawn(|| {
                for round in 0..ROUNDS {
                    add_model_in(
                        &data_dir,
                        &format!("groq/model-{round}"),
                        "groq",
                        "llama-3.1-8b-instant",
                        Some(&SecretKey("sk-add".to_string())),
                        None,
                    )
                    .expect("add");
                }
            });
            scope.spawn(|| {
                for round in 0..ROUNDS {
                    codypendent_runtime::auth::update(&data_dir, |auth| -> anyhow::Result<_> {
                        auth.set(format!("other-{round}"), "sk-other");
                        Ok((Save::Yes, ()))
                    })
                    .expect("update");
                }
            });
        });

        let auth = AuthStore::load(&data_dir).expect("auth.json");
        let lost: Vec<String> = (0..ROUNDS)
            .map(|round| format!("other-{round}"))
            .filter(|id| auth.get(id).is_none())
            .collect();
        assert!(lost.is_empty(), "keys lost by a concurrent add: {lost:?}");
    }

    /// The one rule this module exists to keep: a key can be received and
    /// written, and can never be printed.
    #[test]
    fn a_secret_key_never_appears_in_its_own_debug() {
        let key = SecretKey("sk-not-in-the-output".to_string());
        let rendered = format!("{key:?}");
        assert!(!rendered.contains("sk-not-in-the-output"), "{rendered}");
        assert_eq!(rendered, "SecretKey(<redacted>)");
    }

    /// A key target names an id, never key material — so a debug log of a
    /// failed write cannot leak the key it was writing.
    #[test]
    fn a_key_target_carries_only_a_non_secret_id() {
        let target = KeyTarget::Provider {
            id: "groq".to_string(),
        };
        assert_eq!(target.auth_id(), "provider/groq");
        assert_eq!(
            KeyTarget::Model {
                id: "groq/llama".to_string()
            }
            .auth_id(),
            "groq/llama"
        );
    }

    /// A base URL reaches the chat client as `{base}/chat/completions`, so a
    /// catalog value written with a trailing slash must not persist with one.
    #[test]
    fn a_persisted_base_url_has_no_trailing_slash() {
        assert_eq!(
            normalize_base_url(" https://api.example.com/v1/ "),
            "https://api.example.com/v1"
        );
    }

    /// The gate that decides whether the add flow may write an entry at all.
    #[test]
    fn a_provider_with_no_base_url_is_not_runtime_supported() {
        let provider = Provider {
            id: "oauth-only".to_string(),
            name: "OAuth only".to_string(),
            protocol: Protocol::OpenAiChat,
            base_url: Some("   ".to_string()),
            auth: vec![AuthMethod::None],
            extra_headers: BTreeMap::new(),
            query_params: BTreeMap::new(),
            local: false,
        };
        assert!(!provider_runtime_supported(&provider));
        assert!(!provider_can_list_models(&provider));
    }

    /// The community bridge is a trust decision, and this build cannot make it
    /// succeed — so it must say so rather than look like an ordinary provider.
    #[test]
    fn the_community_bridge_row_is_gated_and_unavailable() {
        let row = community_bridge_row();
        assert!(row.community_consent_required);
        assert!(!row.available);
        assert!(row.community_consent_detail.is_some());
        assert!(row.unusable_reason.is_some());
    }

    /// A `models.toml` a human owns holds more than models. Adding one must
    /// not delete their embedding, retrieval or voice configuration — the bug
    /// `crates/cli/src/models_file.rs` exists to make unrepresentable.
    #[test]
    fn rewriting_the_model_array_preserves_every_other_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("models.toml");
        std::fs::write(
            &path,
            r#"
[[model]]
id = "existing/one"
provider = "openai-compatible"
base_url = "https://api.example.com/v1"
model = "existing-one"
api_key_env = "EXAMPLE_KEY"

[embedding]
provider = "ollama"
base_url = "http://localhost:11434/v1"
model = "nomic-embed-text"

[speech]
base_url = "http://localhost:9000/v1"
model = "tts-1"
voice = "alloy"
"#,
        )
        .expect("seed");

        update_model_entries(&path, |configs| {
            configs.push(ModelConfig {
                id: ModelId("added/two".to_string()),
                provider: "openai-compatible".to_string(),
                base_url: "https://api.example.com/v1".to_string(),
                model: "added-two".to_string(),
                api_key_env: String::new(),
                provider_id: Some("example".to_string()),
                context_tokens: None,
            });
            Ok(())
        })
        .expect("update");

        let raw = std::fs::read_to_string(&path).expect("read back");
        assert!(raw.contains("nomic-embed-text"), "{raw}");
        assert!(raw.contains("tts-1"), "{raw}");
        let configs = load_models(&path).expect("parse");
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[1].id.0, "added/two");
    }
}
