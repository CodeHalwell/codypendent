//! Model providers (STEP 1.9).
//!
//! Three pieces, deliberately kept separate so only one of them depends on a
//! concrete framework provider crate:
//!
//! 1. [`ModelConfig`] / [`load_models`] / [`ModelRegistry`] — parse
//!    `models.toml` and, at call time, build an
//!    `agent_framework_openai::OpenAIChatCompletionClient` for a given
//!    [`ModelId`]. Gated behind the `provider-openai` feature (on by
//!    default), per ADR-009: this crate depends on `agent-framework-rs`
//!    provider crates only behind provider features.
//! 2. [`ModelPolicy`] — the Phase 1 ordered candidate list per
//!    [`AgentMode`]. This is *not* the Phase 7 utility router; it is the
//!    minimal "try this, then that" list called for by STEP 1.9.
//! 3. [`resolve_model`] — walks a policy's candidates for a mode and returns
//!    the first one whose endpoint is reachable, falling back on connection
//!    failure. This does not depend on `provider-openai`: candidate
//!    selection only needs to know whether an endpoint is reachable, not how
//!    to speak its wire protocol, so it stays available even if that feature
//!    is disabled. The caller (the STEP 1.10 agent loop) uses the returned
//!    [`ResolvedModel::id`] both to attribute the run and to obtain the
//!    actual client via `ModelRegistry::client_for`.
//!
//! API keys are read from the configured environment variable at the moment
//! a client is constructed ([`ModelRegistry::client_for`]) — never persisted,
//! never logged, never placed in model context (Chapter 11, "Secrets").

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use codypendent_protocol::{AgentMode, ModelId};
use serde::{Deserialize, Serialize};

#[cfg(feature = "provider-openai")]
use agent_framework_openai::OpenAIChatCompletionClient;

#[cfg(feature = "provider-openai")]
use std::sync::Arc;

#[cfg(feature = "provider-openai")]
use codypendent_providers::{
    credential_for, AuthMethod, CredentialError, Protocol, ResolvedCredential,
};

/// This module's result alias.
pub type Result<T> = std::result::Result<T, ModelsError>;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// One `[[model]]` entry from `<config_dir>/codypendent/models.toml`.
///
/// ```toml
/// [[model]]
/// id = "hosted-default"
/// provider = "openai-compatible"
/// base_url = "https://api.openai.com/v1"
/// model = "gpt-5.1-codex"
/// api_key_env = "OPENAI_API_KEY"   # env var NAME; value never stored
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelConfig {
    /// The [`ModelId`] this profile is selected by (from a [`ModelPolicy`]
    /// candidate list, or directly).
    pub id: ModelId,
    /// The wire protocol adapter to use. Phase 1 supports exactly one value:
    /// `"openai-compatible"` (any OpenAI Chat Completions-wire endpoint —
    /// OpenAI itself, Azure OpenAI, Ollama, together.ai, ...).
    pub provider: String,
    /// The OpenAI-compatible base URL, e.g. `https://api.openai.com/v1` or
    /// `http://localhost:11434/v1`.
    pub base_url: String,
    /// The provider-side model name sent in each request, e.g.
    /// `gpt-5.1-codex` or `qwen2.5-coder:14b`.
    pub model: String,
    /// The NAME of the environment variable holding the API key, read at
    /// call time. Empty string means no key is needed (e.g. a local Ollama
    /// endpoint with no auth).
    #[serde(default)]
    pub api_key_env: String,
}

/// The on-disk shape of `models.toml`: a bare array of `[[model]]` tables.
#[derive(Debug, Deserialize)]
struct ModelsFile {
    #[serde(default, rename = "model")]
    model: Vec<ModelConfig>,
}

/// Parse `models.toml` at `path` into its [`ModelConfig`] entries.
///
/// Exposed standalone (in addition to [`ModelRegistry::load`]) so tests — and
/// callers that want to inspect or filter configs before building a registry
/// — can drive parsing directly against a temp file.
pub fn load_models(path: &Path) -> Result<Vec<ModelConfig>> {
    let text = std::fs::read_to_string(path).map_err(|source| ModelsError::ReadConfig {
        path: path.to_path_buf(),
        source,
    })?;
    let file: ModelsFile = toml::from_str(&text).map_err(|source| ModelsError::ParseConfig {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(file.model)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from model configuration, client construction, and candidate
/// resolution.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ModelsError {
    /// `models.toml` could not be read.
    #[error("failed to read model config file at {path}: {source}")]
    ReadConfig {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// `models.toml` was read but is not valid TOML / does not match the
    /// expected shape.
    #[error("failed to parse model config file at {path}: {source}")]
    ParseConfig {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// A [`ModelId`] was requested that has no entry in the registry.
    #[error("model `{0}` is not registered")]
    UnknownModel(ModelId),

    /// A model's `provider` field named something other than
    /// `"openai-compatible"` — the only provider Phase 1 supports.
    #[error(
        "model `{model}` uses unsupported provider `{provider}` (Phase 1 supports only \"openai-compatible\")"
    )]
    UnsupportedProvider { model: ModelId, provider: String },

    /// A model's provider maps to a wire protocol this build does not yet wire
    /// (Anthropic/Gemini native are follow-ups; only OpenAI-compatible is wired).
    #[error("model `{model}` uses protocol `{protocol}` which is not yet wired (only OpenAI-compatible is)")]
    ProtocolNotWired { model: ModelId, protocol: String },

    /// A model's `api_key_env` names an environment variable that is not
    /// set. Names the variable, per STEP 1.9's test requirement and the
    /// Chapter 11 rule that secrets are identified, never guessed at, in
    /// error output.
    #[error(
        "model `{model}` requires the environment variable `{var}` for its API key, but it is not set"
    )]
    MissingApiKeyEnv { model: ModelId, var: String },

    /// A `base_url` could not be reduced to a `host:port` authority for a
    /// connectivity check.
    #[error("could not parse base_url `{base_url}`: {reason}")]
    InvalidBaseUrl { base_url: String, reason: String },

    /// A connectivity check against `base_url` failed (connection refused,
    /// unreachable, timed out, ...).
    #[error("connection check to `{base_url}` failed: {reason}")]
    ConnectionFailed { base_url: String, reason: String },

    /// [`ModelPolicy::candidates`] returned an empty list for the mode.
    #[error("no candidate model is configured for mode {mode:?}")]
    NoCandidates { mode: AgentMode },

    /// Every candidate for the mode failed to resolve (unregistered or
    /// unreachable). Carries each attempted [`ModelId`] and its failure
    /// reason, in candidate order, for diagnostics.
    #[error("all candidate models for mode {mode:?} failed: {attempts:?}")]
    AllCandidatesFailed {
        mode: AgentMode,
        attempts: Vec<(ModelId, String)>,
    },
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// The set of configured model profiles, keyed by [`ModelId`].
#[derive(Debug, Clone, Default)]
pub struct ModelRegistry {
    configs: HashMap<ModelId, ModelConfig>,
}

impl ModelRegistry {
    /// Build a registry from already-parsed configs. Later entries with a
    /// duplicate `id` overwrite earlier ones.
    pub fn new(configs: impl IntoIterator<Item = ModelConfig>) -> Self {
        let configs = configs.into_iter().map(|c| (c.id.clone(), c)).collect();
        Self { configs }
    }

    /// Parse `models.toml` at `path` and build a registry from it.
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self::new(load_models(path)?))
    }

    /// Look up a model's configuration by id.
    pub fn get(&self, id: &ModelId) -> Option<&ModelConfig> {
        self.configs.get(id)
    }

    /// Iterate over every registered model id.
    pub fn ids(&self) -> impl Iterator<Item = &ModelId> {
        self.configs.keys()
    }
}

/// Map a legacy [`ModelConfig`] onto the new provider abstraction: today's only
/// supported `provider = "openai-compatible"` becomes `(OpenAiChat, ApiKey|None)`.
/// An empty `api_key_env` means no key (local endpoints) → `AuthMethod::None`.
/// This is the backward-compatible bridge that lets every existing
/// `models.toml` keep resolving through the generalized [`ModelRegistry::client_for`].
#[cfg(feature = "provider-openai")]
fn config_to_protocol_auth(cfg: &ModelConfig) -> Result<(Protocol, AuthMethod)> {
    if cfg.provider != "openai-compatible" {
        return Err(ModelsError::UnsupportedProvider {
            model: cfg.id.clone(),
            provider: cfg.provider.clone(),
        });
    }
    let auth = if cfg.api_key_env.trim().is_empty() {
        AuthMethod::None
    } else {
        AuthMethod::ApiKey {
            env: vec![cfg.api_key_env.clone()],
            header: "Authorization".to_string(),
            prefix: "Bearer ".to_string(),
        }
    };
    Ok((Protocol::OpenAiChat, auth))
}

#[cfg(feature = "provider-openai")]
impl ModelRegistry {
    /// Build a framework chat client for `id`, dispatching on the model's wire
    /// [`Protocol`] and resolving credentials through the async
    /// `CredentialProvider` seam (`codypendent_providers::credential_for`).
    ///
    /// Reads the API key from its env var right here, at call time — it is
    /// moved straight into the client and is never stored on the registry,
    /// logged, or otherwise retained by this function (Chapter 11,
    /// "Secrets"). A required-but-unset variable produces
    /// [`ModelsError::MissingApiKeyEnv`] naming the variable.
    ///
    /// Today only [`Protocol::OpenAiChat`] is wired: a legacy `models.toml`
    /// entry (`provider = "openai-compatible"`) maps onto it via
    /// [`config_to_protocol_auth`] and builds the exact same
    /// `OpenAIChatCompletionClient::new(api_key, model).with_base_url(base_url)`
    /// as before — the one code path that serves both the hosted OpenAI
    /// endpoint and any OpenAI-compatible local/self-hosted endpoint (e.g.
    /// Ollama), per STEP 1.9 — now returned behind `Arc<dyn ChatClient>`. Any
    /// other protocol (Anthropic/Gemini native are follow-ups) returns
    /// [`ModelsError::ProtocolNotWired`].
    pub async fn client_for(
        &self,
        id: &ModelId,
    ) -> Result<Arc<dyn agent_framework_core::client::ChatClient>> {
        let cfg = self
            .get(id)
            .ok_or_else(|| ModelsError::UnknownModel(id.clone()))?;
        let (protocol, auth) = config_to_protocol_auth(cfg)?;
        match protocol {
            Protocol::OpenAiChat => {
                let api_key = match credential_for(&auth).resolve().await {
                    Ok(ResolvedCredential::ApiKey { value, .. }) => value,
                    Ok(ResolvedCredential::None) => String::new(),
                    Err(CredentialError::MissingEnv { var }) => {
                        return Err(ModelsError::MissingApiKeyEnv {
                            model: id.clone(),
                            var,
                        });
                    }
                    // `CredentialError` is `#[non_exhaustive]`: this also
                    // catches `NotWired` (unreachable today — the legacy
                    // bridge above only ever produces `ApiKey`/`None` auth,
                    // both wired) plus any future variant.
                    Err(other) => {
                        return Err(ModelsError::ProtocolNotWired {
                            model: id.clone(),
                            protocol: other.to_string(),
                        });
                    }
                };
                let client = OpenAIChatCompletionClient::new(api_key, cfg.model.clone())
                    .with_base_url(cfg.base_url.clone());
                Ok(Arc::new(client))
            }
            other => Err(ModelsError::ProtocolNotWired {
                model: id.clone(),
                protocol: format!("{other:?}"),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 1 model policy
// ---------------------------------------------------------------------------

/// The Phase 1 model policy: an ordered candidate [`ModelId`] list per
/// [`AgentMode`], with an optional fallback list for modes with no explicit
/// entry.
///
/// This is intentionally minimal — a static ordered list, walked in order by
/// [`resolve_model`] until one connects. It is *not* the Phase 7 utility
/// router (cost/latency/quality-aware routing arrives there); see STEP 1.9.
#[derive(Debug, Clone, Default)]
pub struct ModelPolicy {
    per_mode: Vec<(AgentMode, Vec<ModelId>)>,
    default_candidates: Vec<ModelId>,
}

impl ModelPolicy {
    /// An empty policy (every mode resolves to no candidates until
    /// configured).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set (or replace) the ordered candidate list for `mode`.
    pub fn with_candidates(mut self, mode: AgentMode, candidates: impl Into<Vec<ModelId>>) -> Self {
        let candidates = candidates.into();
        match self.per_mode.iter_mut().find(|(m, _)| *m == mode) {
            Some(entry) => entry.1 = candidates,
            None => self.per_mode.push((mode, candidates)),
        }
        self
    }

    /// Set the fallback candidate list used by [`ModelPolicy::candidates`]
    /// for any mode without its own entry.
    pub fn with_default_candidates(mut self, candidates: impl Into<Vec<ModelId>>) -> Self {
        self.default_candidates = candidates.into();
        self
    }

    /// The ordered candidate list for `mode`: its own entry if configured,
    /// otherwise the default list (possibly empty).
    pub fn candidates(&self, mode: AgentMode) -> &[ModelId] {
        self.per_mode
            .iter()
            .find(|(m, _)| *m == mode)
            .map(|(_, c)| c.as_slice())
            .unwrap_or(&self.default_candidates)
    }
}

// ---------------------------------------------------------------------------
// Connectivity probing + resolution
// ---------------------------------------------------------------------------

/// A pluggable "is this endpoint reachable" check, used by [`resolve_model`]
/// to walk a policy's candidates.
///
/// Kept as a small abstraction — rather than hard-coding a real network call
/// inline in the resolution loop — for two reasons: it keeps candidate
/// *selection* free of any dependency on `provider-openai` (a raw TCP check
/// needs to know nothing about the OpenAI wire format), and it makes the
/// fallback-ordering logic in [`resolve_model_with_probe`] deterministically
/// testable without needing a real (and possibly costly) model call. The
/// default implementation, [`TcpConnectProbe`], performs a genuine TCP
/// connect attempt (not a canned/fake result), so the connect-refused test
/// exercises real OS-level connection failure rather than a mocked one.
#[async_trait::async_trait]
pub trait ConnectivityProbe: Send + Sync {
    /// Attempt to reach `base_url`. `Ok(())` means reachable.
    async fn check(&self, base_url: &str) -> Result<()>;
}

/// The default [`ConnectivityProbe`]: a raw TCP connect to the `base_url`'s
/// `host:port`, with a timeout.
///
/// A TCP-level check (rather than a full HTTP request, and deliberately far
/// short of a real chat completion) is intentional: selecting *which* model
/// serves a run should not itself burn API quota or require an already-valid
/// API key, and it needs to work identically for every provider wire format
/// this crate might ever support. Parsing the authority out of `base_url` is
/// done by hand (`str::split`) rather than via a URL-parsing crate because
/// none is available in this crate's dependency set; it handles the
/// `scheme://host[:port]/path` shape `models.toml` uses and is not a general
/// URL parser (e.g. it does not handle bracketed IPv6 literals).
#[derive(Debug, Clone)]
pub struct TcpConnectProbe {
    pub timeout: Duration,
}

impl Default for TcpConnectProbe {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(2),
        }
    }
}

#[async_trait::async_trait]
impl ConnectivityProbe for TcpConnectProbe {
    async fn check(&self, base_url: &str) -> Result<()> {
        let authority = authority_from_base_url(base_url)?;
        match tokio::time::timeout(self.timeout, tokio::net::TcpStream::connect(&authority)).await {
            Ok(Ok(_stream)) => Ok(()),
            Ok(Err(source)) => Err(ModelsError::ConnectionFailed {
                base_url: base_url.to_string(),
                reason: source.to_string(),
            }),
            Err(_elapsed) => Err(ModelsError::ConnectionFailed {
                base_url: base_url.to_string(),
                reason: "connection attempt timed out".to_string(),
            }),
        }
    }
}

/// Reduce a `scheme://host[:port]/path...` base URL to a `host:port`
/// authority suitable for `TcpStream::connect`. Defaults to port 80 for
/// `http://` and 443 for `https://` when no port is given.
fn authority_from_base_url(base_url: &str) -> Result<String> {
    let rest = base_url.split_once("://").map_or(base_url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err(ModelsError::InvalidBaseUrl {
            base_url: base_url.to_string(),
            reason: "no host found in base_url".to_string(),
        });
    }
    let has_explicit_port = authority
        .rsplit_once(':')
        .is_some_and(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()));
    if has_explicit_port {
        Ok(authority.to_string())
    } else {
        let default_port = if base_url.starts_with("https://") {
            443
        } else {
            80
        };
        Ok(format!("{authority}:{default_port}"))
    }
}

/// The outcome of [`resolve_model`]: which candidate was selected, so the
/// caller (the agent loop) can attribute the run to this model id, per
/// STEP 1.9 / STEP 1.10 rule 3 ("every model request records: model id, ...").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub id: ModelId,
}

/// Walk `policy`'s candidates for `mode` in order, using the default
/// [`TcpConnectProbe`], returning the first that is reachable.
///
/// See [`resolve_model_with_probe`] for the fallback semantics and for
/// injecting a different probe.
pub async fn resolve_model(
    registry: &ModelRegistry,
    policy: &ModelPolicy,
    mode: AgentMode,
) -> Result<ResolvedModel> {
    resolve_model_with_probe(registry, policy, mode, &TcpConnectProbe::default()).await
}

/// Walk `policy`'s candidates for `mode` in order. For each candidate: if it
/// has no registry entry, or `probe.check` on its `base_url` fails, move to
/// the next candidate; the first one that connects is returned. If every
/// candidate fails, returns [`ModelsError::AllCandidatesFailed`] carrying
/// every attempt's id and reason, in order.
pub async fn resolve_model_with_probe(
    registry: &ModelRegistry,
    policy: &ModelPolicy,
    mode: AgentMode,
    probe: &dyn ConnectivityProbe,
) -> Result<ResolvedModel> {
    let candidates = policy.candidates(mode);
    if candidates.is_empty() {
        return Err(ModelsError::NoCandidates { mode });
    }
    let mut attempts = Vec::with_capacity(candidates.len());
    for id in candidates {
        let Some(cfg) = registry.get(id) else {
            attempts.push((id.clone(), "model not registered".to_string()));
            continue;
        };
        match probe.check(&cfg.base_url).await {
            Ok(()) => return Ok(ResolvedModel { id: id.clone() }),
            Err(e) => attempts.push((id.clone(), e.to_string())),
        }
    }
    Err(ModelsError::AllCandidatesFailed { mode, attempts })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tokio::net::TcpListener;

    fn model_id(s: &str) -> ModelId {
        ModelId(s.to_string())
    }

    // -- config parse --------------------------------------------------

    #[test]
    fn parses_two_model_entries_from_toml() {
        let mut file = tempfile::NamedTempFile::new().expect("create temp file");
        write!(
            file,
            r#"
[[model]]
id = "hosted-default"
provider = "openai-compatible"
base_url = "https://api.openai.com/v1"
model = "gpt-5.1-codex"
api_key_env = "OPENAI_API_KEY"

[[model]]
id = "local-default"
provider = "openai-compatible"
base_url = "http://localhost:11434/v1"
model = "qwen2.5-coder:14b"
api_key_env = ""
"#
        )
        .expect("write temp file");

        let configs = load_models(file.path()).expect("parse models.toml");
        assert_eq!(configs.len(), 2);

        assert_eq!(configs[0].id, model_id("hosted-default"));
        assert_eq!(configs[0].provider, "openai-compatible");
        assert_eq!(configs[0].base_url, "https://api.openai.com/v1");
        assert_eq!(configs[0].model, "gpt-5.1-codex");
        assert_eq!(configs[0].api_key_env, "OPENAI_API_KEY");

        assert_eq!(configs[1].id, model_id("local-default"));
        assert_eq!(configs[1].base_url, "http://localhost:11434/v1");
        assert_eq!(configs[1].model, "qwen2.5-coder:14b");
        assert_eq!(configs[1].api_key_env, "");

        // ModelRegistry::load goes through the same path and should agree.
        let registry = ModelRegistry::load(file.path()).expect("load registry");
        assert!(registry.get(&model_id("hosted-default")).is_some());
        assert!(registry.get(&model_id("local-default")).is_some());
        assert_eq!(registry.ids().count(), 2);
    }

    // -- missing-env-var --------------------------------------------------

    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn client_for_names_missing_env_var() {
        // A deliberately unique variable name: never set anywhere in this
        // process, so no set_var/remove_var is needed and there is no race
        // with other tests touching global env state.
        let var_name = "CODYPENDENT_TEST_MODELS_RS_UNSET_KEY_9f3c7ab1";
        assert!(
            std::env::var(var_name).is_err(),
            "test precondition: {var_name} must not be set"
        );

        let id = model_id("hosted-default");
        let registry = ModelRegistry::new([ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-5.1-codex".to_string(),
            api_key_env: var_name.to_string(),
        }]);

        // `Arc<dyn ChatClient>` (the `Ok` type) has no `Debug` impl, so
        // `expect_err` (which would need to print it on the `Ok` branch)
        // isn't usable here; `.err().expect(..)` never needs to format `Ok`.
        let err = registry
            .client_for(&id)
            .await
            .err()
            .expect("missing env var must error");
        match &err {
            ModelsError::MissingApiKeyEnv { model, var } => {
                assert_eq!(model, &id);
                assert_eq!(var, var_name);
            }
            other => panic!("expected MissingApiKeyEnv, got {other:?}"),
        }
        assert!(
            err.to_string().contains(var_name),
            "error message must name the variable: {err}"
        );
    }

    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn client_for_allows_empty_api_key_env_for_local_endpoints() {
        let id = model_id("local-default");
        let registry = ModelRegistry::new([ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            model: "qwen2.5-coder:14b".to_string(),
            api_key_env: String::new(),
        }]);

        // Builds an OpenAI-compatible client with no key (Ok is enough — the
        // concrete `model()` accessor is no longer reachable on the returned
        // `Arc<dyn ChatClient>`).
        assert!(
            registry.client_for(&id).await.is_ok(),
            "empty api_key_env is not an error"
        );
    }

    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn client_for_rejects_unsupported_provider() {
        let id = model_id("weird");
        let registry = ModelRegistry::new([ModelConfig {
            id: id.clone(),
            provider: "anthropic-native".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            model: "claude-sonnet-5".to_string(),
            api_key_env: String::new(),
        }]);

        let err = registry
            .client_for(&id)
            .await
            .err()
            .expect("unsupported provider must error");
        assert!(matches!(err, ModelsError::UnsupportedProvider { .. }));
    }

    #[test]
    fn client_for_unknown_model_is_reported() {
        // Exercise the unknown-model path without requiring provider-openai:
        // the underlying registry lookup used by `client_for` is provider
        // agnostic, so this checks it directly via `get`.
        let registry = ModelRegistry::new(Vec::new());
        assert!(registry.get(&model_id("nope")).is_none());
    }

    // -- ModelPolicy --------------------------------------------------

    #[test]
    fn policy_candidates_fall_back_to_default_list() {
        let hosted = model_id("hosted-default");
        let local = model_id("local-default");
        let policy = ModelPolicy::new()
            .with_candidates(AgentMode::Build, vec![hosted.clone(), local.clone()])
            .with_default_candidates(vec![local.clone()]);

        assert_eq!(
            policy.candidates(AgentMode::Build),
            &[hosted, local.clone()]
        );
        // Ask/Explore/etc. have no explicit entry, so they fall back.
        assert_eq!(policy.candidates(AgentMode::Ask), &[local]);
    }

    #[test]
    fn policy_with_no_entries_and_no_default_is_empty() {
        let policy = ModelPolicy::new();
        assert!(policy.candidates(AgentMode::Build).is_empty());
    }

    // -- fallback on connect-refused --------------------------------------

    #[tokio::test]
    async fn resolve_model_falls_back_past_a_closed_port() {
        // Port 1 is a privileged, essentially-never-listening TCP port; a
        // connect attempt against it on localhost gets an immediate OS-level
        // refusal rather than a slow timeout, making this deterministic and
        // fast without mocking anything.
        let closed = model_id("closed-port-candidate");
        // A real listener that accepts the TCP handshake (though nothing
        // speaks HTTP on it) stands in for "reachable". TCP connect()
        // succeeds once the handshake completes and the kernel queues the
        // connection, even with no explicit `accept()` call, so simply
        // keeping the listener bound and alive is enough.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let reachable_addr = listener.local_addr().expect("local_addr");
        let reachable = model_id("reachable-candidate");

        let registry = ModelRegistry::new([
            ModelConfig {
                id: closed.clone(),
                provider: "openai-compatible".to_string(),
                base_url: "http://127.0.0.1:1/v1".to_string(),
                model: "unused".to_string(),
                api_key_env: String::new(),
            },
            ModelConfig {
                id: reachable.clone(),
                provider: "openai-compatible".to_string(),
                base_url: format!("http://{reachable_addr}/v1"),
                model: "unused".to_string(),
                api_key_env: String::new(),
            },
        ]);
        let policy = ModelPolicy::new()
            .with_candidates(AgentMode::Build, vec![closed.clone(), reachable.clone()]);

        let resolved = resolve_model(&registry, &policy, AgentMode::Build)
            .await
            .expect("second candidate should be reachable");
        assert_eq!(resolved.id, reachable);

        drop(listener);
    }

    #[tokio::test]
    async fn resolve_model_reports_structured_error_when_every_candidate_fails() {
        let closed = model_id("closed-port-only");
        let registry = ModelRegistry::new([ModelConfig {
            id: closed.clone(),
            provider: "openai-compatible".to_string(),
            base_url: "http://127.0.0.1:1/v1".to_string(),
            model: "unused".to_string(),
            api_key_env: String::new(),
        }]);
        let policy = ModelPolicy::new().with_candidates(AgentMode::Build, vec![closed.clone()]);

        let err = resolve_model(&registry, &policy, AgentMode::Build)
            .await
            .expect_err("no reachable candidate");
        match err {
            ModelsError::AllCandidatesFailed { mode, attempts } => {
                assert_eq!(mode, AgentMode::Build);
                assert_eq!(attempts.len(), 1);
                assert_eq!(attempts[0].0, closed);
            }
            other => panic!("expected AllCandidatesFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_model_errors_when_no_candidates_configured() {
        let registry = ModelRegistry::new(Vec::new());
        let policy = ModelPolicy::new();
        let err = resolve_model(&registry, &policy, AgentMode::Explore)
            .await
            .expect_err("empty candidate list must error");
        assert!(matches!(err, ModelsError::NoCandidates { .. }));
    }

    #[tokio::test]
    async fn resolve_model_skips_unregistered_candidate_ids() {
        let reachable = model_id("registered-and-reachable");
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let reachable_addr = listener.local_addr().expect("local_addr");

        let registry = ModelRegistry::new([ModelConfig {
            id: reachable.clone(),
            provider: "openai-compatible".to_string(),
            base_url: format!("http://{reachable_addr}/v1"),
            model: "unused".to_string(),
            api_key_env: String::new(),
        }]);
        let ghost = model_id("not-in-registry");
        let policy =
            ModelPolicy::new().with_candidates(AgentMode::Plan, vec![ghost, reachable.clone()]);

        let resolved = resolve_model(&registry, &policy, AgentMode::Plan)
            .await
            .expect("second, registered candidate should resolve");
        assert_eq!(resolved.id, reachable);

        drop(listener);
    }

    // -- authority_from_base_url -------------------------------------------

    #[test]
    fn authority_parsing_handles_explicit_and_default_ports() {
        assert_eq!(
            authority_from_base_url("http://127.0.0.1:1/v1").unwrap(),
            "127.0.0.1:1"
        );
        assert_eq!(
            authority_from_base_url("http://localhost:11434/v1").unwrap(),
            "localhost:11434"
        );
        assert_eq!(
            authority_from_base_url("https://api.openai.com/v1").unwrap(),
            "api.openai.com:443"
        );
        assert_eq!(
            authority_from_base_url("http://example.com").unwrap(),
            "example.com:80"
        );
        assert!(authority_from_base_url("not-a-url").is_ok());
    }
}
