//! Model providers (STEP 1.9).
//!
//! Three pieces, deliberately kept separate so only one of them depends on a
//! concrete framework provider crate:
//!
//! 1. [`ModelConfig`] / [`load_models`] / [`ModelRegistry`] — parse
//!    `models.toml` and, at call time, build an
//!    a governed provider client for a given
//!    [`ModelId`]. Gated behind the `provider-openai` feature (on by
//!    default), per ADR-009: this crate depends on `agent-framework-rs`
//!    provider crates only behind provider features.
//! 2. [`ModelPolicy`] — the Phase 1 ordered candidate list per
//!    [`AgentMode`]. This is *not* the Phase 7 utility router; it is the
//!    minimal "try this, then that" list called for by STEP 1.9.
//! 3. [`resolve_model`] — walks a policy's candidates for a mode and returns
//!    the first model the provider actually reports as available. Merely
//!    accepting a TCP connection is not enough: an Ollama server can be up
//!    while the configured tag is absent. The caller uses the returned
//!    [`ResolvedModel::id`] both to attribute the run and obtain the client.
//!
//! API keys are read from the configured environment variable at the moment
//! a client is constructed ([`ModelRegistry::client_for`]) — never persisted,
//! never logged, never placed in model context (Chapter 11, "Secrets").

use std::collections::HashMap;
#[cfg(feature = "provider-openai")]
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use codypendent_protocol::{AgentMode, ModelId};
use serde::{Deserialize, Serialize};

use crate::auth::AuthStore;
use codypendent_providers::Catalog;

#[cfg(feature = "provider-openai")]
use std::collections::BTreeMap;
#[cfg(feature = "provider-openai")]
use std::sync::Arc;

#[cfg(feature = "provider-openai")]
use codypendent_providers::{
    credential_for, AuthMethod, CloudIamCredential, CredentialError, CredentialProvider,
    OAuthCredential, Protocol, ResolvedCredential, TokenProvider,
};

/// This module's result alias.
pub type Result<T> = std::result::Result<T, ModelsError>;

/// Live provider transports fail a dead connect promptly and do not let a
/// streaming peer hold a request open forever without delivering bytes.
const PROVIDER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PROVIDER_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Output-token ceiling sent to Anthropic when the caller sets none. The API
/// REQUIRES `max_tokens`, and this used to be 4096: a single `write_file` for a
/// ~400-line source file does not fit, so the model's tool call was cut off
/// mid-JSON and the run looped on a confusing "requires a string `path`" error.
/// 16K holds a large file or a long answer and is accepted by every current
/// Claude model (their output ceilings are 64K and up).
#[cfg(feature = "provider-openai")]
const ANTHROPIC_DEFAULT_MAX_TOKENS: u32 = 16_384;

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
    /// The runtime adapter family: `"openai-compatible"` for REST chat
    /// models, or `"acp"` for an external agent from the official ACP
    /// registry. ACP agents own their model and tool loop; [`Self::model`] is
    /// their registry id. `"openai-compatible"` is broader than its name: the
    /// ACTUAL wire protocol (OpenAI chat-completions, Anthropic Messages, ...)
    /// is resolved from [`Self::provider_id`] against the provider catalog at
    /// call time (see `config_to_protocol_auth`), not from this field — an
    /// entry with `provider_id = Some("anthropic")` speaks the Anthropic wire
    /// even though `provider` still reads `"openai-compatible"`.
    pub provider: String,
    /// The OpenAI-compatible base URL. Empty for ACP profiles.
    #[serde(default)]
    pub base_url: String,
    /// Provider-side model name, or the pinned ACP `agent-id@version` coordinate.
    pub model: String,
    /// The NAME of the environment variable holding the API key, read at
    /// call time. Empty string means no key is needed (e.g. a local Ollama
    /// endpoint with no auth).
    #[serde(default)]
    pub api_key_env: String,
    /// The provider-catalog id this model was added from (e.g. `"nebius"`,
    /// `"azure-openai"`), when known. Additive and optional: it lets the
    /// registry resolve the provider's auth header/prefix, extra headers, and
    /// documented key env NAMES from the catalog at call time, so a
    /// non-bearer provider (Azure OpenAI's `api-key` header, GitHub Models'
    /// API-version header) authenticates correctly instead of being
    /// flattened to `Authorization: Bearer`. `None` (every pre-existing
    /// entry) keeps the legacy bearer behavior exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// The model's context window in tokens, if known. Sourced from
    /// `models.toml` (a user-set `context_tokens` key) — there is no
    /// auto-population from the provider catalog in v1. Used for two
    /// things: (1) the `num_ctx` request hint, and (2) the denominator of
    /// the context-usage percentage surfaced in the TUI footer. `None`
    /// means "unknown" — no percentage is fabricated and no `num_ctx` is
    /// sent; a config without this key parses unchanged (back-compatible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
}

/// The on-disk shape of `models.toml`: a bare array of `[[model]]` tables.
#[derive(Debug, Deserialize)]
struct ModelsFile {
    #[serde(default, rename = "model")]
    model: Vec<ModelConfig>,
}

/// A `context_tokens` above this cannot come from any real model this catalog
/// knows about — the largest curated ceiling as of this build is OpenAI's
/// 1,050,000-token tier (`crates/providers/builtin_catalog.toml`); doubling
/// that and rounding up leaves headroom for near-future genuine growth
/// without needing a code change on every model release, while still
/// rejecting anything a hostile or misconfigured gateway would plausibly send
/// (a `u64::MAX`, or any other implausible `"context_length"` in a provider's
/// own `/models` response).
pub const MAX_PLAUSIBLE_CONTEXT_TOKENS: u64 = 2_000_000;

/// Clamp an untrusted `context_tokens` reading to [`MAX_PLAUSIBLE_CONTEXT_TOKENS`].
/// `None` (unknown) passes through unchanged — this only caps an implausible
/// number, it never fabricates one.
///
/// This is the load-bearing half of closing the trust-boundary gap the
/// 2026-08-13 review found (F4, `acp-models.md`): `crates/cli/src/tui.rs`'s
/// `merge_catalog_rows` lets a provider's own `/models` response win over the
/// curated catalog's `context_tokens` on the (reasonable, for every OTHER
/// field) theory that "the provider knows its own model best" — but that
/// value is then persisted into `models.toml` verbatim and, from there, is
/// load-bearing rather than display-only: `FrameworkModelDriver::from_registry`
/// (`crates/runtime/src/agent.rs`) reads it and forwards it as Ollama's
/// `{"options":{"num_ctx": n}}` request hint, and it is the denominator of the
/// TUI footer's context-usage percentage. Clamping here, at the one place
/// every `models.toml` entry is parsed regardless of which writer produced it
/// (the TUI's add-model flow, `codypendent models add`, or a hand-edit), means
/// every downstream consumer is safe even though none of them re-validates —
/// a defense-in-depth floor under the TUI-side and `agent.rs`-side fixes
/// proposed alongside this change (`.impl/proposals/agent-tui-from-agent-models.md`,
/// `.impl/proposals/agent-retrieval-from-agent-models.md`), not a replacement
/// for them: this is a blunt absolute ceiling, not the tighter
/// per-model catalog ceiling [`ModelRegistry::context_tokens_for`] applies
/// when a catalog is attached.
#[must_use]
pub fn clamp_context_tokens(context_tokens: Option<u64>) -> Option<u64> {
    context_tokens.map(|tokens| tokens.min(MAX_PLAUSIBLE_CONTEXT_TOKENS))
}

/// Parse `models.toml` at `path` into its [`ModelConfig`] entries.
///
/// Exposed standalone (in addition to [`ModelRegistry::load`]) so tests — and
/// callers that want to inspect or filter configs before building a registry
/// — can drive parsing directly against a temp file. Every entry's
/// `context_tokens` is passed through [`clamp_context_tokens`] here, so this
/// function — not only [`ModelRegistry`] — never hands a caller an implausible
/// value; a test that parses a fixture file directly (bypassing the registry)
/// is protected exactly like a live daemon load.
pub fn load_models(path: &Path) -> Result<Vec<ModelConfig>> {
    let text = std::fs::read_to_string(path).map_err(|source| ModelsError::ReadConfig {
        path: path.to_path_buf(),
        source,
    })?;
    let file: ModelsFile = toml::from_str(&text).map_err(|source| ModelsError::ParseConfig {
        path: path.to_path_buf(),
        source,
    })?;
    let mut models = file.model;
    for model in &mut models {
        model.context_tokens = clamp_context_tokens(model.context_tokens);
    }
    Ok(models)
}

// ---------------------------------------------------------------------------
// models.toml extras: the `[embedding]` entry + `[retrieval]` tuning
// ---------------------------------------------------------------------------

/// The `[embedding]` entry in `models.toml` (rubric 9 — real embeddings):
/// names the OpenAI-compatible `/embeddings` endpoint retrieval embeds with.
/// Absent, retrieval keeps the offline hashing embedder (today's behavior).
///
/// ```toml
/// [embedding]
/// provider = "openai-compatible"          # the default; may be omitted
/// base_url = "http://localhost:11434/v1"  # Ollama, OpenAI, Nebius, …
/// model = "nomic-embed-text"
/// api_key_env = ""                        # env var NAME; value never stored
/// dims = 768                              # optional: verified against responses
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    /// The runtime adapter; only `"openai-compatible"` is wired.
    #[serde(default = "default_embedding_provider")]
    pub provider: String,
    /// The OpenAI-compatible base URL (`POST {base_url}/embeddings`).
    pub base_url: String,
    /// Provider-side embedding model name (e.g. `nomic-embed-text`,
    /// `Qwen/Qwen3-Embedding-8B`).
    pub model: String,
    /// The NAME of the environment variable holding the API key, read at call
    /// time — never persisted (the `ModelConfig::api_key_env` contract). Empty
    /// means no key (a local endpoint).
    #[serde(default)]
    pub api_key_env: String,
    /// Expected vector dimensionality. `None` accepts whatever the model
    /// returns; `Some(n)` rejects a mismatched response (a wrong-model guard).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dims: Option<usize>,
}

fn default_embedding_provider() -> String {
    "openai-compatible".to_string()
}

/// Default `[retrieval] mcp_top_k`: when a run's MCP bridge offers more than
/// this many tools, only the `mcp_top_k` most relevant to the run are
/// advertised (at or below it, all are — today's behavior). `0` disables the
/// gate entirely (full injection).
pub const DEFAULT_MCP_TOP_K: usize = 8;

/// Default `[retrieval] builtin_top_k`: how many BUILT-IN tools the retrieval
/// funnel selects for a run on top of the always-advertised floor
/// (`codypendent_runtime::agent::ALWAYS_ADVERTISED_TOOLS`). `0` disables the gate
/// entirely (advertise every offered built-in — full injection, the behavior
/// before rubric 9 reached this family).
///
/// Eight matches [`DEFAULT_MCP_TOP_K`] and the `skills.search` card budget, and
/// with the seven-tool floor it lands a default Build run well under the full
/// catalog rather than at all of it.
pub const DEFAULT_BUILTIN_TOP_K: usize = 8;

/// The `[retrieval]` tuning table in `models.toml`.
///
/// ```toml
/// [retrieval]
/// mcp_top_k = 8       # 0 disables retrieval gating (advertise every MCP tool)
/// builtin_top_k = 8   # 0 disables it for the built-in tools
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalSettings {
    /// See [`DEFAULT_MCP_TOP_K`].
    #[serde(default = "default_mcp_top_k")]
    pub mcp_top_k: usize,
    /// See [`DEFAULT_BUILTIN_TOP_K`].
    #[serde(default = "default_builtin_top_k")]
    pub builtin_top_k: usize,
}

fn default_mcp_top_k() -> usize {
    DEFAULT_MCP_TOP_K
}

fn default_builtin_top_k() -> usize {
    DEFAULT_BUILTIN_TOP_K
}

impl Default for RetrievalSettings {
    fn default() -> Self {
        Self {
            mcp_top_k: DEFAULT_MCP_TOP_K,
            builtin_top_k: DEFAULT_BUILTIN_TOP_K,
        }
    }
}

/// Tuning for live LSP diagnostics feedback (Phase 4 follow-up: Chapter 07).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LspSettings {
    /// Whether live LSP diagnostics feedback is enabled on write/edit tools.
    /// Defaults to `true` (opt-out).
    #[serde(default = "default_lsp_enabled")]
    pub enabled: bool,
}

fn default_lsp_enabled() -> bool {
    true
}

impl Default for LspSettings {
    fn default() -> Self {
        Self {
            enabled: default_lsp_enabled(),
        }
    }
}

/// The non-`[[model]]` tables of `models.toml`, parsed independently of
/// [`load_models`] so both readers ignore each other's keys and an existing
/// file is back-compatible in both directions.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ModelExtras {
    /// The `[embedding]` entry, when configured.
    #[serde(default)]
    pub embedding: Option<EmbeddingConfig>,
    /// The `[retrieval]` tuning; every field defaults when the table is absent.
    #[serde(default)]
    pub retrieval: RetrievalSettings,
    /// The `[lsp]` tuning; defaults to enabled when the table is absent.
    #[serde(default)]
    pub lsp: LspSettings,
}

/// Parse the `[embedding]` / `[retrieval]` extras from `models.toml`. An
/// ABSENT file yields the defaults (no embedding model, default tuning) —
/// unlike [`load_models`], because these tables are optional configuration and
/// a daemon with no `models.toml` at all must still assemble context. A
/// present-but-malformed file is an error, so a typo is legible rather than a
/// silent fallback.
pub fn load_model_extras(path: &Path) -> Result<ModelExtras> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ModelExtras::default())
        }
        Err(source) => {
            return Err(ModelsError::ReadConfig {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    toml::from_str(&text).map_err(|source| ModelsError::ParseConfig {
        path: path.to_path_buf(),
        source,
    })
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

    /// A model's `provider` field names no installed runtime adapter.
    #[error(
        "model `{model}` uses unsupported provider `{provider}` (supported: \"openai-compatible\", \"acp\")"
    )]
    UnsupportedProvider { model: ModelId, provider: String },

    /// A model's provider maps to a wire protocol this build does not wire a
    /// client for. The `provider-openai` transport feature wires OpenAI chat,
    /// Anthropic Messages, and Gemini native; anything else is a follow-up.
    /// Also returned for a wired protocol when
    /// the enabling feature was compiled out (`--no-default-features`), so a
    /// passing [`ModelRegistry::check_model`] never promises more than
    /// [`ModelRegistry::client_for`] can deliver.
    #[error("model `{model}` uses protocol `{protocol}` which is not wired in this build")]
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

    /// The endpoint answered, but the configured provider-side model was not
    /// present in its OpenAI-compatible `/models` catalog.
    #[error("model `{model}` (`{provider_model}`) is unavailable: {reason}")]
    ModelUnavailable {
        model: ModelId,
        provider_model: String,
        reason: String,
    },

    /// [`ModelPolicy::candidates`] returned an empty list for the mode.
    #[error("no candidate model is configured for mode {mode:?}")]
    NoCandidates { mode: AgentMode },

    /// Every candidate for the mode failed to resolve (unregistered or
    /// unreachable). Carries each attempted [`ModelId`] and its failure
    /// reason, in candidate order, for diagnostics.
    #[error(
        "all candidate models for mode {mode:?} failed: {}",
        describe_attempts(.attempts)
    )]
    AllCandidatesFailed {
        mode: AgentMode,
        attempts: Vec<CandidateFailure>,
    },
}

/// One candidate's failure inside [`ModelsError::AllCandidatesFailed`].
///
/// `classified` is kept ALONGSIDE the text rather than re-derived from it.
/// Stringifying at aggregation time is what made the aggregate lie: four
/// models all missing their key arrived as one opaque
/// `AllCandidatesFailed`, and a classifier reading only that could say
/// nothing better than "reconfigure the model" — sending a person to Models
/// when the answer was API Keys.
#[derive(Debug, Clone)]
pub struct CandidateFailure {
    /// The candidate that failed.
    pub id: ModelId,
    /// Its failure, as a person reads it.
    pub reason: String,
    /// Its classified cause, when it had a typed one.
    pub classified: Option<codypendent_protocol::CodypendentError>,
}

impl CandidateFailure {
    /// A candidate whose failure is a typed [`ModelsError`].
    pub fn of(id: ModelId, error: &ModelsError) -> Self {
        Self {
            id,
            reason: error.to_string(),
            classified: Some(classify_models_error(error)),
        }
    }

    /// A candidate whose failure has no typed cause this crate can read — an
    /// external agent's launch refusal, say. It still names its reason; it
    /// simply cannot contribute a classification, which the aggregate treats
    /// as "no agreement" rather than inventing one.
    pub fn untyped(id: ModelId, reason: String) -> Self {
        Self {
            id,
            reason,
            classified: None,
        }
    }

    /// A candidate that is named by the policy but absent from the registry —
    /// a configuration fault, and one this crate states rather than infers.
    pub fn unregistered(id: ModelId) -> Self {
        let mut classified = codypendent_protocol::CodypendentError::new(
            "model.not-registered",
            format!("model `{}` is named by the policy but not registered", id.0),
            false,
        );
        classified.user_action = Some(codypendent_protocol::UserAction::ReconfigureModel);
        Self {
            id,
            reason: "model not registered".to_string(),
            classified: Some(classified),
        }
    }
}

/// One `id: reason` per candidate, `;`-separated. This text reaches a person —
/// the failed run's reason, `models check`, `doctor` — and the previous
/// `{attempts:?}` printed a Rust tuple vector (`[(ModelId("x"), "…")]`) at them.
/// The structured half of a failed run: a stable code, retryability and the
/// affordance a client should offer, read off the TYPED cause in the error
/// chain — the framework's `ServiceInvalidAuth` / `ServiceStatus` /
/// `ServiceInvalidRequest` / `ServiceContentFilter`, or this crate's own
/// [`ModelsError`] — never off the message text. `None` when nothing typed is
/// in the chain: the client then falls back to reading `reason`, as before.
pub fn classify_run_failure(
    error: &anyhow::Error,
) -> Option<codypendent_protocol::CodypendentError> {
    use codypendent_protocol::{CodypendentError, UserAction};

    let classified = |code: &str, message: String, retryable: bool, action: Option<UserAction>| {
        let mut error = CodypendentError::new(code, message, retryable);
        error.user_action = action;
        error
    };
    for cause in error.chain() {
        if let Some(framework) = cause.downcast_ref::<agent_framework_core::Error>() {
            use agent_framework_core::Error as Framework;
            return match framework {
                Framework::ServiceInvalidAuth { message } => Some(classified(
                    "model.invalid-auth",
                    format!("the provider refused the credential: {message}"),
                    false,
                    Some(UserAction::Reauthenticate),
                )),
                Framework::ServiceStatus {
                    status, message, ..
                } => Some(match status {
                    408 | 425 | 429 | 500 | 502 | 503 | 504 | 529 => classified(
                        "model.unavailable",
                        format!("the provider is unavailable ({status}): {message}"),
                        true,
                        Some(UserAction::Retry),
                    ),
                    _ => classified(
                        "model.rejected",
                        format!("the provider rejected the request ({status}): {message}"),
                        false,
                        Some(UserAction::ReconfigureModel),
                    ),
                }),
                Framework::ServiceInvalidRequest { message } => Some(classified(
                    "model.invalid-request",
                    format!("the provider rejected the request: {message}"),
                    false,
                    Some(UserAction::ReconfigureModel),
                )),
                Framework::ServiceContentFilter { message } => Some(classified(
                    "model.content-filter",
                    format!("the provider's content filter refused the request: {message}"),
                    false,
                    None,
                )),
                _ => None,
            };
        }
        if let Some(models) = cause.downcast_ref::<ModelsError>() {
            return Some(classify_models_error(models));
        }
    }
    None
}

/// Classify ONE model-layer failure. Split out of [`classify_run_failure`] so
/// the aggregate below can reuse it per candidate rather than re-deriving a
/// verdict from an already-flattened message — and public so a caller holding
/// the typed error directly (the daemon's run-startup path) classifies through
/// exactly this, never through the message text.
pub fn classify_models_error(models: &ModelsError) -> codypendent_protocol::CodypendentError {
    use codypendent_protocol::{CodypendentError, UserAction};

    let classified = |code: &str, message: String, retryable: bool, action: Option<UserAction>| {
        let mut error = CodypendentError::new(code, message, retryable);
        error.user_action = action;
        error
    };
    match models {
        ModelsError::MissingApiKeyEnv { model, var } => classified(
            "model.missing-credential",
            format!("{model} needs a credential: {var} is not set and no key is stored"),
            false,
            Some(UserAction::Reauthenticate),
        ),
        ModelsError::ConnectionFailed { base_url, reason } => classified(
            "model.unreachable",
            format!("could not reach {base_url}: {reason}"),
            true,
            Some(UserAction::ReconfigureModel),
        ),
        // Every candidate failed. What to DO about that is only as clear as
        // the agreement underneath it, so it is decided there, not here.
        ModelsError::AllCandidatesFailed { attempts, .. } => {
            aggregate_candidate_failures(models, attempts)
        }
        // NOT grouped with the permanent causes below. `failure_class` already
        // reads this variant's provider-supplied reason and calls a 429 or 5xx
        // from `/models` transient; saying `retryable: false` here made the two
        // functions disagree about the SAME error, and told a client to go and
        // reconfigure a model whose only problem was that the provider was busy.
        // Retryability is derived from the same reason, so they cannot drift.
        ModelsError::ModelUnavailable { reason, .. } => {
            let transient = matches!(classify_provider_message(reason), FailureClass::Transient);
            classified(
                if transient {
                    "model.unavailable"
                } else {
                    "model.not-runnable"
                },
                models.to_string(),
                transient,
                Some(if transient {
                    UserAction::Retry
                } else {
                    UserAction::ReconfigureModel
                }),
            )
        }
        ModelsError::InvalidBaseUrl { .. }
        | ModelsError::UnknownModel(_)
        | ModelsError::UnsupportedProvider { .. }
        | ModelsError::ProtocolNotWired { .. }
        | ModelsError::NoCandidates { .. } => classified(
            "model.not-runnable",
            models.to_string(),
            false,
            Some(UserAction::ReconfigureModel),
        ),
        other => classified("model.error", other.to_string(), false, None),
    }
}

/// The verdict for a whole failed candidate list.
///
/// When every candidate failed the SAME way, that way IS the answer: four
/// models all missing their key is a credential problem, and the client should
/// offer API Keys. When they disagree, no single next step is right, so the
/// aggregate carries a code and its retryability but NO `user_action`, and
/// the client falls back to reading the message — exactly what it did before
/// any of this classification existed. Claiming an affordance here that only
/// one candidate justified is worse than claiming none.
fn aggregate_candidate_failures(
    aggregate: &ModelsError,
    attempts: &[CandidateFailure],
) -> codypendent_protocol::CodypendentError {
    use codypendent_protocol::{CodypendentError, UserAction};

    let action_of = |attempt: &CandidateFailure| -> Option<UserAction> {
        attempt.classified.as_ref().and_then(|c| c.user_action)
    };
    fn code_of(attempt: &CandidateFailure) -> Option<&str> {
        attempt.classified.as_ref().map(|c| c.code.as_str())
    }
    let unanimous =
        |first: Option<UserAction>| attempts.iter().all(|attempt| action_of(attempt) == first);

    let agreed_action = attempts
        .first()
        .map(action_of)
        .filter(|first| first.is_some() && unanimous(*first))
        .flatten();
    let agreed_code = attempts
        .first()
        .and_then(code_of)
        .filter(|code| {
            attempts
                .iter()
                .all(|attempt| code_of(attempt) == Some(*code))
        })
        .map(str::to_owned);
    // Retryable when ANY candidate is. `retryable` asks whether an identical
    // retry could succeed, and an identical retry re-walks the WHOLE candidate
    // list — so one model permanently missing its key does not stop a local
    // fallback from coming back and serving the run. Requiring all of them
    // would report a recoverable set as permanent and suppress a retry that
    // would have worked.
    let retryable = attempts
        .iter()
        .any(|attempt| attempt.classified.as_ref().is_some_and(|c| c.retryable));

    let mut error = CodypendentError::new(
        agreed_code.unwrap_or_else(|| "model.all-candidates-failed".to_string()),
        aggregate.to_string(),
        retryable,
    );
    error.user_action = if attempts.is_empty() {
        // Nothing was tried at all, which is a configuration fault.
        Some(UserAction::ReconfigureModel)
    } else {
        agreed_action
    };
    error
}

fn describe_attempts(attempts: &[CandidateFailure]) -> String {
    if attempts.is_empty() {
        return "no candidate was tried".to_owned();
    }
    attempts
        .iter()
        .map(|attempt| format!("{}: {}", attempt.id, attempt.reason))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Whether a model-request failure is worth retrying (the transient/permanent
/// error taxonomy the loop's retry-with-backoff wrapper dispatches on — see
/// `FrameworkModelDriver::next_step` in `agent.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// Environmental and time-bounded — the identical request may genuinely
    /// succeed a moment later (connect refused/reset, timeouts, HTTP 408/429/
    /// 5xx, provider overload). Worth a bounded retry.
    Transient,
    /// The request itself (or its configuration) is refused — bad credentials,
    /// an unknown model, a 4xx contract violation, unparseable config. A retry
    /// can only repeat the same refusal, so it is surfaced immediately.
    Permanent,
}

impl ModelsError {
    /// Classify this error for retry purposes. Only the genuinely
    /// environmental variants are [`FailureClass::Transient`]:
    /// [`ConnectionFailed`](ModelsError::ConnectionFailed) (refused/reset/
    /// timed-out connection attempts) always is, and
    /// [`ModelUnavailable`](ModelsError::ModelUnavailable) is when its
    /// provider-supplied `reason` names a transient condition (an HTTP 5xx or
    /// 429 from `/models`). Everything else — config read/parse failures,
    /// unknown models, unsupported providers, missing credentials, exhausted
    /// candidate lists — is [`FailureClass::Permanent`]: retrying re-reads the
    /// same file and re-misses the same env var.
    #[must_use]
    pub fn failure_class(&self) -> FailureClass {
        match self {
            ModelsError::ConnectionFailed { .. } => FailureClass::Transient,
            ModelsError::ModelUnavailable { reason, .. } => classify_provider_message(reason),
            _ => FailureClass::Permanent,
        }
    }
}

/// Classify an OPAQUE provider/stream failure message as transient or
/// permanent. The live driver's stream errors cross the framework seam as
/// formatted strings (and `ModelUnavailable.reason` embeds a response body),
/// so this is a conservative textual taxonomy: only a message that plainly
/// names a transient condition — a connect/reset/timeout failure, provider
/// overload, or an HTTP 408/429/5xx status — is [`FailureClass::Transient`];
/// anything unrecognized is [`FailureClass::Permanent`], so a novel failure
/// surfaces immediately instead of being silently retried. HTTP status codes
/// are matched as standalone digit runs (never substrings), so a `500` inside
/// an id like `15005` cannot misclassify a message.
#[must_use]
pub fn classify_provider_message(message: &str) -> FailureClass {
    match codypendent_providers::retry::retryable(message) {
        Some(_) => FailureClass::Transient,
        None => FailureClass::Permanent,
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// The `auth.json` entry id holding a PROVIDER-wide key (as opposed to a
/// per-model entry keyed by the model's display id): `provider/<catalog-id>`.
/// Stored by the add-model flow alongside the per-model entry so one pasted
/// key serves every model later added from the same provider; read by
/// [`ModelRegistry::client_for`]/[`check_model`](ModelRegistry::check_model)
/// after the per-model entry and before the environment. The `provider/`
/// prefix is reserved (like the Tavily `integrations/tavily` id) and cannot
/// collide with add-model display ids, which are `<provider>/<model>` for
/// catalog providers and `acp/<agent>` for ACP agents.
#[must_use]
pub fn provider_auth_id(provider_id: &str) -> String {
    format!("provider/{provider_id}")
}

/// The process-wide built-in provider catalog, parsed once. The fallback for
/// registries built without [`ModelRegistry::with_catalog`] (e.g. the daemon
/// executor), so catalog-declared auth headers resolve there too.
fn builtin_catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(Catalog::builtin)
}

/// The set of configured model profiles, keyed by [`ModelId`], plus the
/// resolved [`AuthStore`] (`auth.json`) so [`ModelRegistry::client_for`] can
/// prefer a stored key over the model's `api_key_env`. The store's own redacting
/// `Debug` keeps the derived `Debug` here from leaking a key.
#[derive(Clone, Default)]
pub struct ModelRegistry {
    configs: HashMap<ModelId, ModelConfig>,
    auth: AuthStore,
    /// The provider catalog auth headers are resolved against (see
    /// [`ModelConfig::provider_id`]). `None` falls back to the built-ins, so
    /// a caller that cannot layer the user's `providers.toml` still resolves
    /// every built-in provider correctly.
    #[cfg_attr(not(feature = "provider-openai"), allow(dead_code))]
    catalog: Option<Catalog>,
    #[cfg(feature = "provider-openai")]
    token_providers: HashMap<String, Arc<dyn TokenProvider>>,
    #[cfg(feature = "provider-openai")]
    credential_resolvers: Arc<tokio::sync::Mutex<HashMap<String, Arc<dyn CredentialProvider>>>>,
}

impl std::fmt::Debug for ModelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("ModelRegistry");
        debug
            .field("configs", &self.configs)
            .field("auth", &self.auth)
            .field("catalog", &self.catalog);
        #[cfg(feature = "provider-openai")]
        debug
            .field("token_providers", &"<opaque>")
            .field("credential_resolvers", &"<opaque>");
        debug.finish()
    }
}

impl ModelRegistry {
    /// Build a registry from already-parsed configs. Later entries with a
    /// duplicate `id` overwrite earlier ones. The auth store starts empty (no
    /// `auth.json` keys), so every model resolves exactly as before until one is
    /// attached with [`with_auth`](Self::with_auth).
    pub fn new(configs: impl IntoIterator<Item = ModelConfig>) -> Self {
        let configs = configs.into_iter().map(|c| (c.id.clone(), c)).collect();
        Self {
            configs,
            auth: AuthStore::default(),
            catalog: None,
            #[cfg(feature = "provider-openai")]
            token_providers: HashMap::new(),
            #[cfg(feature = "provider-openai")]
            credential_resolvers: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Attach the resolved [`AuthStore`] (`auth.json`) so `client_for` prefers a
    /// stored key over the model's `api_key_env`. Additive: the default empty
    /// store leaves every model resolving exactly as before.
    #[must_use]
    pub fn with_auth(mut self, auth: AuthStore) -> Self {
        self.auth = auth;
        self
    }

    /// Attach a resolved provider [`Catalog`] (built-ins layered with the
    /// user's `providers.toml`) for auth-header resolution. Additive: without
    /// it the embedded built-in catalog is used, so only a user-defined
    /// provider with a custom auth header needs this to be exact.
    #[must_use]
    pub fn with_catalog(mut self, catalog: Catalog) -> Self {
        self.catalog = Some(catalog);
        self
    }

    /// Inject a non-interactive delegated token source for one catalog provider.
    #[cfg(feature = "provider-openai")]
    #[must_use]
    pub fn with_token_provider(
        mut self,
        provider_id: impl Into<String>,
        provider: Arc<dyn TokenProvider>,
    ) -> Self {
        self.token_providers.insert(provider_id.into(), provider);
        self.credential_resolvers = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        self
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

    /// Whether this profile delegates the run to an ACP agent.
    #[must_use]
    pub fn is_acp(&self, id: &ModelId) -> bool {
        self.get(id).is_some_and(|config| config.provider == "acp")
    }

    /// The full ACP coordinate for an ACP profile — `id`, `id@version`, or
    /// `id@version#model` once the profile pins one of the agent's own models.
    /// Launch-oriented consumers strip the additive `#model` part with
    /// [`acp_coordinate_agent`]; [`acp_agent_model`](Self::acp_agent_model)
    /// reads the pin.
    #[must_use]
    pub fn acp_agent_id(&self, id: &ModelId) -> Option<&str> {
        self.get(id)
            .filter(|config| config.provider == "acp")
            .map(|config| config.model.as_str())
    }

    /// The agent-model id an ACP profile pins on its external agent
    /// (`id@version#model`), if any. `None` for a bare agent profile — the
    /// agent then keeps whatever default model it booted with.
    #[must_use]
    pub fn acp_agent_model(&self, id: &ModelId) -> Option<&str> {
        self.acp_agent_id(id).and_then(acp_coordinate_model)
    }
}

/// Split an ACP model coordinate `id[@version][#model]` at the first `#` and
/// return the launchable agent part. Additive: coordinates written before
/// model pinning existed carry no `#` and return themselves unchanged.
#[must_use]
pub fn acp_coordinate_agent(coordinate: &str) -> &str {
    coordinate
        .split_once('#')
        .map_or(coordinate, |(agent, _)| agent)
}

/// The agent-model id an ACP coordinate pins (`id[@version]#model`), if any.
/// A dangling `#` pins nothing.
#[must_use]
pub fn acp_coordinate_model(coordinate: &str) -> Option<&str> {
    coordinate
        .split_once('#')
        .map(|(_, model)| model)
        .filter(|model| !model.is_empty())
}

#[cfg(test)]
mod acp_coordinate_tests {
    use super::*;

    #[test]
    fn coordinates_split_into_agent_and_optional_model_pin() {
        assert_eq!(acp_coordinate_agent("demo-acp@1.2.3"), "demo-acp@1.2.3");
        assert_eq!(acp_coordinate_model("demo-acp@1.2.3"), None);
        assert_eq!(
            acp_coordinate_agent("demo-acp@1.2.3#agent-model-1"),
            "demo-acp@1.2.3"
        );
        assert_eq!(
            acp_coordinate_model("demo-acp@1.2.3#agent-model-1"),
            Some("agent-model-1")
        );
        // A dangling separator pins nothing (and still names the agent).
        assert_eq!(acp_coordinate_model("demo-acp@1.2.3#"), None);
        assert_eq!(acp_coordinate_agent("demo-acp@1.2.3#"), "demo-acp@1.2.3");
    }

    #[test]
    fn registry_reads_the_model_pin_from_acp_profiles_only() {
        let registry = ModelRegistry::new([
            ModelConfig {
                id: ModelId("acp/demo#agent-model-1".to_string()),
                provider: "acp".to_string(),
                base_url: String::new(),
                model: "demo-acp@1.2.3#agent-model-1".to_string(),
                api_key_env: String::new(),
                context_tokens: None,
                provider_id: None,
            },
            ModelConfig {
                id: ModelId("acp/demo".to_string()),
                provider: "acp".to_string(),
                base_url: String::new(),
                model: "demo-acp@1.2.3".to_string(),
                api_key_env: String::new(),
                context_tokens: None,
                provider_id: None,
            },
            ModelConfig {
                id: ModelId("hosted".to_string()),
                provider: "openai-compatible".to_string(),
                base_url: "https://example.test/v1".to_string(),
                model: "some#model".to_string(),
                api_key_env: String::new(),
                context_tokens: None,
                provider_id: None,
            },
        ]);
        assert_eq!(
            registry.acp_agent_model(&ModelId("acp/demo#agent-model-1".to_string())),
            Some("agent-model-1")
        );
        assert_eq!(
            registry.acp_agent_model(&ModelId("acp/demo".to_string())),
            None
        );
        // Non-ACP profiles never expose a pin, whatever their model string.
        assert_eq!(
            registry.acp_agent_model(&ModelId("hosted".to_string())),
            None
        );
    }
}

/// How a model's endpoint expects its key and extra headers on the wire,
/// resolved from the provider catalog via [`ModelConfig::provider_id`]. A
/// config with no (known) provider id resolves to the bearer default, so
/// every pre-existing `models.toml` entry behaves exactly as before.
#[cfg(feature = "provider-openai")]
#[derive(Debug, Clone)]
struct EndpointAuth {
    /// The header the key is injected under (default `Authorization`).
    header: String,
    /// The value prefix in front of the key (default `"Bearer "`).
    prefix: String,
    /// Provider-wide headers sent on every request — an API-version pin, for
    /// the providers whose catalog entry declares one.
    extra_headers: BTreeMap<String, String>,
    /// Provider-wide query parameters sent on every native request.
    query_params: BTreeMap<String, String>,
    /// The provider's documented key env-var NAMES, consulted (first set
    /// wins) only when the model has no `auth.json` key and no explicit
    /// `api_key_env` of its own.
    provider_env: Vec<String>,
    /// The catalog provider declares API-key auth, even when its env-name
    /// list is empty. Kept separately from `provider_env` so readiness never
    /// mistakes a malformed key-auth provider for a deliberately keyless one.
    requires_api_key: bool,
}

/// Resolve a model's [`EndpointAuth`] from the catalog. Defaults to bearer
/// with no extras when the entry names no provider, or an unknown one.
#[cfg(feature = "provider-openai")]
fn endpoint_auth_for(cfg: &ModelConfig, catalog: &Catalog) -> EndpointAuth {
    let provider = cfg.provider_id.as_deref().and_then(|id| catalog.get(id));
    let (header, prefix, provider_env, requires_api_key) = provider
        .and_then(|p| {
            p.auth.iter().find_map(|method| match method {
                AuthMethod::ApiKey {
                    env,
                    header,
                    prefix,
                } => Some((header.clone(), prefix.clone(), env.clone(), true)),
                _ => None,
            })
        })
        .unwrap_or_else(|| {
            (
                "Authorization".to_string(),
                "Bearer ".to_string(),
                Vec::new(),
                false,
            )
        });
    EndpointAuth {
        header,
        prefix,
        extra_headers: provider
            .map(|p| p.extra_headers.clone())
            .unwrap_or_default(),
        query_params: provider.map(|p| p.query_params.clone()).unwrap_or_default(),
        provider_env,
        requires_api_key,
    }
}

/// Map a persisted [`ModelConfig`] onto the provider abstraction. Chat profiles
/// become `(protocol, ApiKey|None)`; ACP profiles are marked so the assembly
/// executor can route them to the full-agent runtime instead of a chat client.
///
/// `provider == "openai-compatible"` on [`ModelConfig`] is a broad "REST chat
/// family" adapter marker, not a promise that the wire is literally OpenAI's:
/// the catalog is the authority on the actual wire [`Protocol`]. When the
/// entry names a known [`ModelConfig::provider_id`], that provider's declared
/// `protocol` wins (e.g. `anthropic` resolves to [`Protocol::Anthropic`], not
/// OpenAI chat-completions) — this is what lets `codypendent models add
/// anthropic claude-opus-5` build a client that actually speaks the Anthropic
/// Messages API instead of POSTing an OpenAI-shaped body to it. An absent or
/// unknown `provider_id` keeps today's [`Protocol::OpenAiChat`] default, so
/// every pre-existing `models.toml` entry (and every entry from a provider
/// this catalog doesn't curate) resolves exactly as before. The `ApiKey` arm
/// carries the catalog-resolved header/prefix (bearer when the entry names no
/// provider) so Azure- and Anthropic-shaped providers stop being flattened to
/// `Authorization: Bearer`.
#[cfg(feature = "provider-openai")]
fn config_to_protocol_auth(cfg: &ModelConfig, catalog: &Catalog) -> Result<(Protocol, AuthMethod)> {
    if cfg.provider == "acp" {
        return Ok((
            Protocol::Acp,
            AuthMethod::Acp {
                command: String::new(),
                args: Vec::new(),
                env: Default::default(),
            },
        ));
    }
    if cfg.provider != "openai-compatible" {
        return Err(ModelsError::UnsupportedProvider {
            model: cfg.id.clone(),
            provider: cfg.provider.clone(),
        });
    }
    let protocol = cfg
        .provider_id
        .as_deref()
        .and_then(|id| catalog.get(id))
        .map(|provider| provider.protocol)
        .unwrap_or(Protocol::OpenAiChat);
    let auth = if cfg.api_key_env.trim().is_empty() {
        AuthMethod::None
    } else {
        let endpoint = endpoint_auth_for(cfg, catalog);
        AuthMethod::ApiKey {
            env: vec![cfg.api_key_env.clone()],
            header: endpoint.header,
            prefix: endpoint.prefix,
        }
    };
    Ok((protocol, auth))
}

#[cfg(feature = "provider-openai")]
impl ModelRegistry {
    /// The catalog auth headers are resolved against: the attached one, or
    /// the process-wide built-ins.
    fn catalog(&self) -> &Catalog {
        self.catalog.as_ref().unwrap_or_else(|| builtin_catalog())
    }

    /// `id`'s `context_tokens`, clamped against the tighter of two ceilings:
    /// [`MAX_PLAUSIBLE_CONTEXT_TOKENS`] (always, via [`clamp_context_tokens`])
    /// and — when `id` names a catalog-known `provider_id` + provider-side
    /// `model` — that exact row's own curated `context_tokens`, when the
    /// catalog documents one. The second is strictly tighter: a curated
    /// Anthropic row tops out at 1,000,000, so a live `/models` response that
    /// claimed 1,900,000 for that same model would pass the blunt absolute
    /// ceiling but should not pass THIS one. Callers that forward
    /// `context_tokens` somewhere consequential (the Ollama `num_ctx` request
    /// hint, a context-usage percentage) should prefer this over reading
    /// [`ModelConfig::context_tokens`] directly — see F4 in
    /// `2026-08-13-verticals/acp-models.md` and [`clamp_context_tokens`]'s
    /// doc comment for the full trust-boundary account.
    #[must_use]
    pub fn context_tokens_for(&self, id: &ModelId) -> Option<u64> {
        let cfg = self.get(id)?;
        let configured = clamp_context_tokens(cfg.context_tokens)?;
        let catalog_ceiling = cfg
            .provider_id
            .as_deref()
            .and_then(|provider_id| self.catalog().model(provider_id, &cfg.model))
            .and_then(|row| row.context_tokens);
        Some(match catalog_ceiling {
            Some(ceiling) => configured.min(ceiling),
            None => configured,
        })
    }

    /// Resolve the key exactly as the live client does. Keeping this in one
    /// helper ensures the readiness probe and the first completion cannot
    /// disagree because they used different credential precedence:
    /// (1) the model's own `auth.json` entry; (2) the provider-wide
    /// `auth.json` entry ([`provider_auth_id`]); (3) the model's explicit
    /// `api_key_env` (missing → a naming error, unchanged); (4) the catalog
    /// provider's documented env NAMES, best-effort — an unset variable
    /// falls through to keyless exactly as before, never a new error.
    async fn api_key_for(&self, cfg: &ModelConfig) -> Result<String> {
        if let Some(key) = self
            .auth
            .get(cfg.id.0.as_str())
            .filter(|key| !key.trim().is_empty())
        {
            return Ok(key.to_string());
        }
        if let Some(key) = cfg
            .provider_id
            .as_deref()
            .and_then(|pid| self.auth.get(&provider_auth_id(pid)))
            .filter(|key| !key.trim().is_empty())
        {
            return Ok(key.to_string());
        }
        let (_, auth) = config_to_protocol_auth(cfg, self.catalog())?;
        match credential_for(&auth).resolve().await {
            Ok(ResolvedCredential::ApiKey { value, .. }) => Ok(value),
            Ok(ResolvedCredential::None) => {
                let endpoint = endpoint_auth_for(cfg, self.catalog());
                if !endpoint.provider_env.is_empty() {
                    let provider_auth = AuthMethod::ApiKey {
                        env: endpoint.provider_env,
                        header: endpoint.header,
                        prefix: endpoint.prefix,
                    };
                    if let Ok(ResolvedCredential::ApiKey { value, .. }) =
                        credential_for(&provider_auth).resolve().await
                    {
                        return Ok(value);
                    }
                }
                Ok(String::new())
            }
            Ok(ResolvedCredential::BearerToken { value, .. }) => Ok(value),
            Err(CredentialError::MissingEnv { var }) => Err(ModelsError::MissingApiKeyEnv {
                model: cfg.id.clone(),
                var,
            }),
            Err(other) => Err(ModelsError::ProtocolNotWired {
                model: cfg.id.clone(),
                protocol: other.to_string(),
            }),
        }
    }

    /// The PERSISTENT credential resolver for a model whose catalog provider
    /// delegates auth (`cloud_iam`/`o_auth`), or `None` when it does not.
    ///
    /// The resolver — not the token it produced — is what has to be shared and
    /// kept: it owns the single-flight cache and the expiry/refresh rule
    /// (`DelegatedCredential`), so a token that expires mid-run is re-minted on
    /// the next resolve. Readiness, the `/models` probe and every live client
    /// therefore go through the SAME resolver, keyed by provider + method.
    async fn delegated_resolver_for(
        &self,
        cfg: &ModelConfig,
    ) -> Result<Option<Arc<dyn CredentialProvider>>> {
        let Some(provider_id) = cfg.provider_id.as_deref() else {
            return Ok(None);
        };
        let Some(method) = self.catalog().get(provider_id).and_then(|p| p.auth.first()) else {
            return Ok(None);
        };
        if !matches!(
            method,
            AuthMethod::CloudIam { .. } | AuthMethod::OAuth { .. }
        ) {
            return Ok(None);
        }
        let Some(provider) = self.token_providers.get(provider_id).cloned() else {
            return Err(ModelsError::ProtocolNotWired {
                model: cfg.id.clone(),
                protocol: "delegated credential requires an injected token provider".into(),
            });
        };
        let cache_key = format!("{provider_id}:{method:?}");
        let mut cache = self.credential_resolvers.lock().await;
        if let Some(existing) = cache.get(&cache_key) {
            return Ok(Some(existing.clone()));
        }
        let resolver: Arc<dyn CredentialProvider> = match method {
            AuthMethod::CloudIam {
                variant, scopes, ..
            } => Arc::new(CloudIamCredential::new(
                variant.clone(),
                scopes.clone(),
                provider,
            )),
            AuthMethod::OAuth {
                client_id, scopes, ..
            } => Arc::new(OAuthCredential::new(
                client_id.clone(),
                scopes.clone(),
                provider,
            )),
            _ => unreachable!("guarded by the matches! above"),
        };
        cache.insert(cache_key, resolver.clone());
        Ok(Some(resolver))
    }

    async fn delegated_credential_for(
        &self,
        cfg: &ModelConfig,
    ) -> Result<Option<ResolvedCredential>> {
        let Some(resolver) = self.delegated_resolver_for(cfg).await? else {
            return Ok(None);
        };
        resolver
            .resolve()
            .await
            .map(Some)
            .map_err(|e| ModelsError::ProtocolNotWired {
                model: cfg.id.clone(),
                protocol: e.to_string(),
            })
    }

    /// Build one native (Anthropic/Gemini) client, authenticated the same way
    /// the readiness probe authenticates.
    ///
    /// A delegated provider is resolved ONCE here so a broken delegation still
    /// fails when the client is built, and the resolver is then handed to the
    /// client so a token that expires mid-run refreshes instead of being
    /// frozen into an immutable header map.
    #[cfg(feature = "provider-openai")]
    async fn native_client(
        &self,
        cfg: &ModelConfig,
        protocol: NativeProtocol,
        auth: &EndpointAuth,
    ) -> Result<NativeChatClient> {
        let resolver = self.delegated_resolver_for(cfg).await?;
        let credential = match &resolver {
            Some(resolver) => {
                resolver
                    .resolve()
                    .await
                    .map_err(|e| ModelsError::ProtocolNotWired {
                        model: cfg.id.clone(),
                        protocol: e.to_string(),
                    })?
            }
            None => ResolvedCredential::ApiKey {
                header: auth.header.clone(),
                prefix: auth.prefix.clone(),
                value: self.api_key_for(cfg).await?,
            },
        };
        let client = NativeChatClient::new_with_credential(cfg, protocol, credential, auth)?;
        Ok(match resolver {
            Some(resolver) => client.with_credential_refresh(resolver),
            None => client,
        })
    }

    /// Whether the configured model can resolve every credential required to
    /// start a run, without exposing the credential itself.
    ///
    /// This deliberately delegates to [`Self::api_key_for`] so discovery and
    /// the live client share one precedence rule: model `auth.json`, provider
    /// `auth.json`, explicit model env, then catalog provider env. A keyless
    /// endpoint is ready when neither the model nor its catalog provider
    /// declares API-key authentication. ACP launchability is checked by its
    /// owning integration; it has no chat credential to resolve here.
    pub async fn credentials_resolvable(&self, id: &ModelId) -> Result<bool> {
        let cfg = self
            .get(id)
            .ok_or_else(|| ModelsError::UnknownModel(id.clone()))?;
        let (protocol, _) = config_to_protocol_auth(cfg, self.catalog())?;
        if matches!(protocol, Protocol::Acp) {
            return Ok(true);
        }

        if let Some(credential) = self.delegated_credential_for(cfg).await? {
            return Ok(matches!(credential, ResolvedCredential::BearerToken { .. }));
        }

        let endpoint = endpoint_auth_for(cfg, self.catalog());
        let requires_api_key = !cfg.api_key_env.trim().is_empty() || endpoint.requires_api_key;
        if !requires_api_key {
            return Ok(true);
        }

        self.api_key_for(cfg)
            .await
            .map(|key| !key.trim().is_empty())
    }

    /// Verify that a configured model is genuinely usable enough to select:
    /// credentials resolve, the OpenAI-compatible `/models` endpoint answers,
    /// and its catalog contains the configured provider-side model name.
    pub async fn check_model(&self, id: &ModelId) -> Result<()> {
        let cfg = self
            .get(id)
            .ok_or_else(|| ModelsError::UnknownModel(id.clone()))?;
        if cfg.provider == "acp" {
            if cfg.model.trim().is_empty() {
                return Err(ModelsError::ModelUnavailable {
                    model: id.clone(),
                    provider_model: cfg.model.clone(),
                    reason: "ACP registry id is blank".to_string(),
                });
            }
            // The assembly executor performs the real cached-registry,
            // executable, and ACP-handshake readiness check because it owns the
            // RuntimePaths needed to resolve the installed agent.
            return Ok(());
        }
        let (protocol, _) = config_to_protocol_auth(cfg, self.catalog())?;
        // The reachability probe's path differs by wire protocol: OpenAI-chat
        // providers list models at `{base_url}/models` (`base_url` already
        // carries the `/v1` the provider documents, e.g.
        // `https://api.openai.com/v1`); Anthropic's Messages API base_url is
        // bare (`https://api.anthropic.com`) and lists models at the
        // spec-fixed `/v1/models` — both return the same `{"data":
        // [{"id": ...}, ...]}` shape, so the matching logic below is shared.
        // Anything else this build cannot wire a client for should not claim
        // to be checkable either — a passing `check` must mean the run that
        // follows can actually build a client.
        let models_suffix = match protocol {
            Protocol::OpenAiChat => "/models",
            Protocol::Anthropic => "/v1/models",
            Protocol::GeminiNative => "/models",
            _ => {
                return Err(ModelsError::ProtocolNotWired {
                    model: id.clone(),
                    protocol: format!("{protocol:?}"),
                });
            }
        };
        if cfg.base_url.trim().is_empty() {
            return Err(ModelsError::InvalidBaseUrl {
                base_url: cfg.base_url.clone(),
                reason: "base_url is blank".to_string(),
            });
        }

        // Resolve delegated auth once for this entire check. The resolver is
        // persistent on the registry, so readiness, this probe, and the live
        // client all share its valid cached token.
        let delegated = self.delegated_credential_for(cfg).await?;
        let key = if delegated.is_none() {
            self.api_key_for(cfg).await?
        } else {
            String::new()
        };
        let endpoint = format!("{}{models_suffix}", cfg.base_url.trim_end_matches('/'));
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .map_err(|error| ModelsError::ConnectionFailed {
                base_url: cfg.base_url.clone(),
                reason: error.to_string(),
            })?;
        // The exact header/prefix + extra headers the live client sends, so
        // an Azure-shaped provider (`api-key`) verifies with the same auth it
        // will run with — never a hardcoded bearer.
        let auth = endpoint_auth_for(cfg, self.catalog());
        let mut request = client.get(endpoint).query(&auth.query_params);
        for (name, value) in &auth.extra_headers {
            request = request.header(name, value);
        }
        let header = match delegated {
            Some(ResolvedCredential::BearerToken { value, .. }) => {
                Some(("Authorization", format!("Bearer {value}")))
            }
            Some(_) => {
                return Err(ModelsError::ProtocolNotWired {
                    model: id.clone(),
                    protocol: "delegated credential did not resolve to a bearer token".into(),
                });
            }
            None if !key.is_empty() => {
                Some((auth.header.as_str(), format!("{}{key}", auth.prefix)))
            }
            None => None,
        };
        if let Some((header, raw_value)) = header {
            let mut value = reqwest::header::HeaderValue::from_str(&raw_value).map_err(|_| {
                ModelsError::ModelUnavailable {
                    model: id.clone(),
                    provider_model: cfg.model.clone(),
                    reason: "the credential is not a valid header value".to_string(),
                }
            })?;
            // Sensitive: reqwest redacts it from any error/debug output.
            value.set_sensitive(true);
            request = request.header(header, value);
        }
        let response = request
            .send()
            .await
            .map_err(|error| ModelsError::ConnectionFailed {
                base_url: cfg.base_url.clone(),
                reason: error.to_string(),
            })?;
        if !response.status().is_success() {
            return Err(ModelsError::ModelUnavailable {
                model: id.clone(),
                provider_model: cfg.model.clone(),
                reason: format!("provider returned HTTP {} from /models", response.status()),
            });
        }
        let payload: serde_json::Value =
            response
                .json()
                .await
                .map_err(|error| ModelsError::ModelUnavailable {
                    model: id.clone(),
                    provider_model: cfg.model.clone(),
                    reason: format!("provider returned an invalid /models response: {error}"),
                })?;
        let available = payload
            .get("data")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("id").and_then(serde_json::Value::as_str))
            .any(|candidate| candidate == cfg.model)
            || payload
                .get("models")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|entry| entry.get("name").and_then(serde_json::Value::as_str))
                .any(|candidate| {
                    candidate.strip_prefix("models/").unwrap_or(candidate) == cfg.model
                });
        if !available {
            return Err(ModelsError::ModelUnavailable {
                model: id.clone(),
                provider_model: cfg.model.clone(),
                reason: "provider did not list this model".to_string(),
            });
        }
        Ok(())
    }

    /// Build a framework chat client for `id`, dispatching on the model's wire
    /// [`Protocol`] and resolving credentials through the async
    /// `CredentialProvider` seam (`codypendent_providers::credential_for`).
    ///
    /// Resolves the API key right here, at call time, in precedence order:
    /// (1) `auth.json[model_id]`, when present and non-empty; (2) the
    /// provider-wide `auth.json` entry ([`provider_auth_id`]); (3) the
    /// model's `api_key_env` environment variable; (4) the catalog
    /// provider's documented env NAMES (best-effort); (5) no key at all
    /// (local endpoints with an empty `api_key_env`). Whichever wins is moved
    /// straight into the client and is never stored on the registry, logged,
    /// or otherwise retained by this function (Chapter 11, "Secrets"). A
    /// required-but-unset variable produces [`ModelsError::MissingApiKeyEnv`]
    /// naming the variable.
    ///
    /// When this build carries `provider-openai` (on by default),
    /// [`Protocol::OpenAiChat`], [`Protocol::Anthropic`], and
    /// [`Protocol::GeminiNative`] are wired. A legacy `models.toml` entry
    /// (`provider = "openai-compatible"`)
    /// maps onto one or the other via [`config_to_protocol_auth`], which
    /// consults the catalog: no `provider_id`, or one this build doesn't
    /// recognize, keeps the OpenAiChat default and builds the wire-compatible
    /// header-aware client — the one code path that serves both hosted OpenAI
    /// endpoint and any OpenAI-compatible local/self-hosted endpoint (e.g.
    /// Ollama), per STEP 1.9 — now returned behind `Arc<dyn ChatClient>`. A
    /// [`ModelConfig::provider_id`] whose catalog auth is not the bearer
    /// default (or that declares extra headers) builds the wire-identical
    /// [`HeaderAuthChatClient`] instead, so Azure OpenAI / GitHub Models
    /// authenticate with the headers the provider actually expects.
    /// `provider_id = "anthropic"` (or any catalog provider declaring
    /// `protocol = "anthropic"`) builds an `agent_framework_anthropic::
    /// AnthropicClient` directly — it speaks the real Messages API wire
    /// (`x-api-key` + `anthropic-version`, its own request/response and SSE
    /// shapes) rather than being flattened through the OpenAI-chat path.
    /// Gemini native and any other undeclared protocol still return
    /// [`ModelsError::ProtocolNotWired`].
    pub async fn client_for(
        &self,
        id: &ModelId,
    ) -> Result<Arc<dyn agent_framework_core::client::ChatClient>> {
        let cfg = self
            .get(id)
            .ok_or_else(|| ModelsError::UnknownModel(id.clone()))?;
        let (protocol, _) = config_to_protocol_auth(cfg, self.catalog())?;
        match protocol {
            Protocol::OpenAiChat => {
                // Key resolution precedence (additive): (a) an `auth.json` key for
                // this model id wins → (b) the provider-wide `auth.json` entry →
                // (c) the model's `api_key_env` (today's path) → (d) none. A model
                // with no `auth.json` entry behaves exactly as before. The stored
                // key is moved straight into the client and is never logged or
                // retained by this function.
                let api_key = self.api_key_for(cfg).await?;
                let auth = endpoint_auth_for(cfg, self.catalog());
                // Use the same custom transport for the ordinary bearer case as
                // for provider-specific headers. Besides being wire-identical,
                // this lets Codypendent own connect/read-idle timeouts and
                // bounded provider error bodies.
                let client = HeaderAuthChatClient::new(cfg, &auth, &api_key).ok_or_else(|| {
                    ModelsError::ModelUnavailable {
                        model: id.clone(),
                        provider_model: cfg.model.clone(),
                        reason: format!(
                            "provider auth header `{}` or its value is not a valid HTTP header",
                            auth.header
                        ),
                    }
                })?;
                Ok(Arc::new(client))
            }
            // Same key-resolution precedence as the OpenAiChat arm above;
            // `AnthropicClient` sends `x-api-key`/`anthropic-version` itself; it
            // is not routed through `HeaderAuthChatClient` because it is not an
            // OpenAI-chat-completions body over different headers — the request
            // and response shapes themselves differ (Messages API), so the
            // framework's purpose-built client owns the whole wire.
            Protocol::Anthropic => {
                let auth = endpoint_auth_for(cfg, self.catalog());
                Ok(Arc::new(
                    self.native_client(cfg, NativeProtocol::Anthropic, &auth)
                        .await?,
                ))
            }
            Protocol::GeminiNative => {
                let auth = endpoint_auth_for(cfg, self.catalog());
                Ok(Arc::new(
                    self.native_client(cfg, NativeProtocol::Gemini, &auth)
                        .await?,
                ))
            }
            Protocol::Acp => Err(ModelsError::ProtocolNotWired {
                model: id.clone(),
                protocol: "acp (full-agent executor, not ChatClient)".to_string(),
            }),
            other => Err(ModelsError::ProtocolNotWired {
                model: id.clone(),
                protocol: format!("{other:?}"),
            }),
        }
    }
}

#[cfg(feature = "provider-openai")]
#[derive(Clone, Copy)]
enum NativeProtocol {
    Anthropic,
    Gemini,
}

#[cfg(feature = "provider-openai")]
fn gemini_synthetic_call_id() -> String {
    format!("gemini-synthetic-{}", uuid::Uuid::now_v7())
}

/// One provider's token counters, normalized onto the framework's field names.
///
/// A counter the provider omitted stays `None` — UNMEASURED, never a
/// fabricated zero, which is the same honesty rule the run's `ModelUsage`
/// applies. Without this the native clients returned no usage at all and every
/// routed run reported null tokens and null cost.
#[cfg(feature = "provider-openai")]
#[derive(Default, Clone, Copy, PartialEq, Eq)]
struct NativeUsage {
    input: Option<u64>,
    output: Option<u64>,
    cache_creation: Option<u64>,
    cache_read: Option<u64>,
    reasoning: Option<u64>,
}

#[cfg(feature = "provider-openai")]
impl NativeUsage {
    /// Read the usage object out of a response/event envelope: Anthropic's
    /// `usage` (`message_start` nests it under `message`, so the caller passes
    /// that object) or Gemini's `usageMetadata`. `None` when the provider sent
    /// no usage object, or one with no counter this build understands.
    fn read(protocol: NativeProtocol, envelope: &serde_json::Value) -> Option<Self> {
        fn count(usage: &serde_json::Value, key: &str) -> Option<u64> {
            usage.get(key).and_then(serde_json::Value::as_u64)
        }
        let parsed = match protocol {
            NativeProtocol::Anthropic => {
                let usage = envelope.get("usage")?;
                Self {
                    input: count(usage, "input_tokens"),
                    output: count(usage, "output_tokens"),
                    cache_creation: count(usage, "cache_creation_input_tokens"),
                    cache_read: count(usage, "cache_read_input_tokens"),
                    reasoning: None,
                }
            }
            NativeProtocol::Gemini => {
                let usage = envelope.get("usageMetadata")?;
                let candidates = count(usage, "candidatesTokenCount");
                let thoughts = count(usage, "thoughtsTokenCount");
                Self {
                    input: count(usage, "promptTokenCount"),
                    // Gemini reports thinking tokens OUTSIDE `candidatesTokenCount`;
                    // the framework's contract (mirroring the OpenAI mapping) is
                    // that reasoning is a SUBSET of the output count, so they are
                    // summed here and also reported on their own field below.
                    output: match (candidates, thoughts) {
                        (None, None) => None,
                        (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
                    },
                    cache_creation: None,
                    cache_read: count(usage, "cachedContentTokenCount"),
                    reasoning: thoughts,
                }
            }
        };
        (parsed != Self::default()).then_some(parsed)
    }

    fn into_details(self) -> agent_framework_core::types::UsageDetails {
        agent_framework_core::types::UsageDetails {
            input_token_count: self.input,
            output_token_count: self.output,
            total_token_count: match (self.input, self.output) {
                (None, None) => None,
                (input, output) => Some(input.unwrap_or(0).saturating_add(output.unwrap_or(0))),
            },
            cache_creation_input_token_count: self.cache_creation,
            cache_read_input_token_count: self.cache_read,
            reasoning_output_token_count: self.reasoning,
            ..Default::default()
        }
    }
}

/// The running totals already emitted for one streamed message.
///
/// Both native providers report usage as RUNNING TOTALS — Anthropic on
/// `message_start` and again on `message_delta`, Gemini on every chunk — while
/// the framework ACCUMULATES every `Content::Usage` it absorbs. Emitting the
/// raw totals would therefore multiply-count them, so this emits only the
/// increment since the last event, and nothing at all when nothing advanced.
#[cfg(feature = "provider-openai")]
#[derive(Default)]
struct UsageTally {
    input: u64,
    output: u64,
    cache_creation: u64,
    cache_read: u64,
    reasoning: u64,
}

#[cfg(feature = "provider-openai")]
impl UsageTally {
    fn advance(
        &mut self,
        reported: NativeUsage,
    ) -> Option<agent_framework_core::types::UsageDetails> {
        fn step(seen: &mut u64, reported: Option<u64>) -> Option<u64> {
            let reported = reported?;
            let delta = reported.saturating_sub(*seen);
            *seen = (*seen).max(reported);
            (delta > 0).then_some(delta)
        }
        let advanced = NativeUsage {
            input: step(&mut self.input, reported.input),
            output: step(&mut self.output, reported.output),
            cache_creation: step(&mut self.cache_creation, reported.cache_creation),
            cache_read: step(&mut self.cache_read, reported.cache_read),
            reasoning: step(&mut self.reasoning, reported.reasoning),
        };
        (advanced != NativeUsage::default()).then(|| advanced.into_details())
    }
}

#[cfg(feature = "provider-openai")]
struct NativeChatClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    protocol: NativeProtocol,
    headers: reqwest::header::HeaderMap,
    query_params: BTreeMap<String, String>,
    credential_raw: Option<String>,
    credential_rendered: Option<String>,
    /// A delegated provider's PERSISTENT resolver, kept so a token that expires
    /// mid-run is re-minted per request instead of being frozen into
    /// `headers` at construction. `None` for API-key auth, which never expires.
    refresh: Option<CredentialRefresh>,
}

/// The refresh half of a delegated credential: the resolver plus the header
/// value last sent, so a change can be REPORTED (never the token itself).
#[cfg(feature = "provider-openai")]
struct CredentialRefresh {
    provider: Arc<dyn CredentialProvider>,
    last_sent: std::sync::Mutex<String>,
}

#[cfg(feature = "provider-openai")]
impl std::fmt::Debug for NativeChatClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeChatClient")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("protocol", &"<native>")
            .field("headers", &"<redacted>")
            .field("query_params", &"<redacted>")
            .field("credential", &"<redacted>")
            .finish()
    }
}

#[cfg(feature = "provider-openai")]
impl NativeChatClient {
    #[cfg(test)]
    fn new(cfg: &ModelConfig, protocol: NativeProtocol, key: &str) -> Result<Self> {
        let header = match protocol {
            NativeProtocol::Anthropic => "x-api-key",
            NativeProtocol::Gemini => "x-goog-api-key",
        };
        Self::new_with_credential(
            cfg,
            protocol,
            ResolvedCredential::ApiKey {
                header: header.into(),
                prefix: String::new(),
                value: key.into(),
            },
            &EndpointAuth {
                header: header.into(),
                prefix: String::new(),
                extra_headers: BTreeMap::new(),
                query_params: BTreeMap::new(),
                provider_env: Vec::new(),
                requires_api_key: true,
            },
        )
    }

    fn new_with_credential(
        cfg: &ModelConfig,
        protocol: NativeProtocol,
        credential: ResolvedCredential,
        auth: &EndpointAuth,
    ) -> Result<Self> {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        for (name, value) in &auth.extra_headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                ModelsError::ModelUnavailable {
                    model: cfg.id.clone(),
                    provider_model: cfg.model.clone(),
                    reason: "provider extra header name is invalid".into(),
                }
            })?;
            let value =
                HeaderValue::from_str(value).map_err(|_| ModelsError::ModelUnavailable {
                    model: cfg.id.clone(),
                    provider_model: cfg.model.clone(),
                    reason: "provider extra header value is invalid".into(),
                })?;
            headers.insert(name, value);
        }
        let (name, credential_raw, rendered) = match credential {
            ResolvedCredential::ApiKey {
                header,
                prefix,
                value,
            } => {
                let rendered = format!("{prefix}{value}");
                (header, Some(value), Some(rendered))
            }
            ResolvedCredential::BearerToken { value, .. } => {
                let rendered = format!("Bearer {value}");
                ("authorization".into(), Some(value), Some(rendered))
            }
            ResolvedCredential::None => ("authorization".into(), None, None),
        };
        let raw = rendered.as_deref().unwrap_or_default();
        let mut value = HeaderValue::from_str(raw).map_err(|_| ModelsError::ModelUnavailable {
            model: cfg.id.clone(),
            provider_model: cfg.model.clone(),
            reason: "credential is not a valid header value".into(),
        })?;
        value.set_sensitive(true);
        if !raw.is_empty() {
            headers.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                    ModelsError::ModelUnavailable {
                        model: cfg.id.clone(),
                        provider_model: cfg.model.clone(),
                        reason: "credential header name is invalid".into(),
                    }
                })?,
                value,
            );
        }
        if matches!(protocol, NativeProtocol::Anthropic)
            && !headers.contains_key("anthropic-version")
        {
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(PROVIDER_CONNECT_TIMEOUT)
                .read_timeout(PROVIDER_IDLE_TIMEOUT)
                .build()
                .map_err(|error| ModelsError::ConnectionFailed {
                    base_url: cfg.base_url.clone(),
                    reason: error.to_string(),
                })?,
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone(),
            protocol,
            headers,
            query_params: auth.query_params.clone(),
            credential_raw,
            credential_rendered: rendered,
            refresh: None,
        })
    }

    /// Keep the delegated resolver so every request re-resolves through its
    /// cache: valid tokens are reused, an expired one is refreshed.
    fn with_credential_refresh(mut self, provider: Arc<dyn CredentialProvider>) -> Self {
        let last_sent = self.credential_rendered.clone().unwrap_or_default();
        self.refresh = Some(CredentialRefresh {
            provider,
            last_sent: std::sync::Mutex::new(last_sent),
        });
        self
    }

    /// Re-resolve a delegated credential for this request, returning the
    /// `Authorization` value to send. Reports a REFRESH (that one happened —
    /// never the token) when the resolver hands back a different token.
    async fn refreshed_authorization(&self) -> agent_framework_core::error::Result<Option<String>> {
        use agent_framework_core::error::Error;
        let Some(refresh) = &self.refresh else {
            return Ok(None);
        };
        let credential = refresh.provider.resolve().await.map_err(|_| {
            // The credential error is classified, never the token; keep the
            // public message free of both.
            Error::service("delegated credential could not be refreshed")
        })?;
        let ResolvedCredential::BearerToken { value, .. } = credential else {
            return Err(Error::service(
                "delegated credential did not resolve to a bearer token",
            ));
        };
        let rendered = format!("Bearer {value}");
        let changed = {
            let mut last = refresh
                .last_sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let changed = *last != rendered;
            if changed {
                rendered.clone_into(&mut last);
            }
            changed
        };
        if changed {
            tracing::debug!(
                model = %self.model,
                "delegated credential refreshed for native chat client"
            );
        }
        Ok(Some(rendered))
    }

    fn body(
        &self,
        messages: &[agent_framework_core::types::Message],
        options: &agent_framework_core::types::ChatOptions,
    ) -> agent_framework_core::error::Result<serde_json::Value> {
        use agent_framework_core::error::Error;
        use agent_framework_core::tools::ToolKind;
        use agent_framework_core::types::{Content, ToolMode};
        if options.frequency_penalty.is_some()
            || options.logit_bias.is_some()
            || options.presence_penalty.is_some()
            || options.seed.is_some()
            || options.store.is_some()
            || options.user.is_some()
            || options.response_format.is_some()
            || !options.additional_properties.is_empty()
        {
            return Err(Error::service_invalid_request(
                "native provider cannot represent one or more requested options",
            ));
        }
        if options.allow_multiple_tool_calls == Some(false) && !options.tools.is_empty() {
            return Err(Error::service_invalid_request(
                "native provider cannot disable parallel tool calls",
            ));
        }
        for tool in &options.tools {
            if !matches!(tool.kind, ToolKind::Function) {
                return Err(Error::service_invalid_request(
                    "native provider supports ordinary function tools only",
                ));
            }
        }
        let system_parts = messages
            .iter()
            .filter(|m| m.role.as_str() == "system")
            .map(|m| m.text())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        let system = match (&options.instructions, system_parts.is_empty()) {
            (Some(i), false) => Some(format!("{i}\n\n{}", system_parts.join("\n\n"))),
            (Some(i), true) => Some(i.clone()),
            (None, false) => Some(system_parts.join("\n\n")),
            (None, true) => None,
        };
        let mut call_names = HashMap::<String, String>::new();
        for call in messages
            .iter()
            .flat_map(|message| message.contents.iter())
            .filter_map(Content::as_function_call)
        {
            if let Some(previous) = call_names.insert(call.call_id.clone(), call.name.clone()) {
                if previous != call.name {
                    return Err(Error::service_invalid_request(format!(
                        "ambiguous function call id `{}`",
                        call.call_id
                    )));
                }
            }
        }
        let content_json = |m: &agent_framework_core::types::Message,
                            anthropic: bool|
         -> agent_framework_core::error::Result<Vec<serde_json::Value>> {
            let mut parts = Vec::new();
            for content in &m.contents {
                match content {
                    Content::Text(t) => parts.push(if anthropic {
                        serde_json::json!({"type":"text","text":t.text})
                    } else {
                        serde_json::json!({"text":t.text})
                    }),
                    Content::FunctionCall(c) => {
                        let args = c
                            .parse_arguments()
                            .map_err(|e| Error::service_invalid_request(e.to_string()))?;
                        parts.push(if anthropic { serde_json::json!({"type":"tool_use","id":c.call_id,"name":c.name,"input":args}) } else { serde_json::json!({"functionCall":{"name":c.name,"args":args,"id":c.call_id}}) });
                    }
                    Content::FunctionResult(r) => {
                        let name = call_names.get(&r.call_id).ok_or_else(|| {
                            Error::service_invalid_request(format!(
                                "function result `{}` has no unique prior function call",
                                r.call_id
                            ))
                        })?;
                        let result = r
                            .result
                            .clone()
                            .unwrap_or_else(|| serde_json::json!({"error":r.exception}));
                        parts.push(if anthropic {
                            let content = match result {
                                serde_json::Value::String(value) => value,
                                value => serde_json::to_string(&value).map_err(|e| Error::service_invalid_request(e.to_string()))?,
                            };
                            serde_json::json!({"type":"tool_result","tool_use_id":r.call_id,"content":content})
                        } else { serde_json::json!({"functionResponse":{"name":name,"response":{"result":result},"id":r.call_id}}) });
                    }
                    _ => {
                        return Err(Error::service_invalid_request(
                            "native provider cannot represent message content type",
                        ))
                    }
                }
            }
            Ok(parts)
        };
        match self.protocol {
            NativeProtocol::Anthropic => {
                let mut body = serde_json::json!({
                    "model": options.model.as_deref().unwrap_or(&self.model),
                    "max_tokens": options.max_tokens.unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS),
                    "messages": messages.iter().filter(|m| m.role.as_str() != "system").map(|m| Ok(serde_json::json!({"role": if m.role.as_str() == "assistant" {"assistant"} else {"user"}, "content": content_json(m, true)?}))).collect::<agent_framework_core::error::Result<Vec<_>>>()?
                });
                if let Some(system) = system {
                    body["system"] = serde_json::json!(system);
                }
                if !options.tools.is_empty() {
                    body["tools"] = serde_json::json!(options.tools.iter().map(|t| serde_json::json!({"name":t.name,"description":t.description,"input_schema":t.parameters})).collect::<Vec<_>>());
                }
                if let Some(choice) = &options.tool_choice {
                    body["tool_choice"] = match choice {
                        ToolMode::Auto => serde_json::json!({"type":"auto"}),
                        ToolMode::None => serde_json::json!({"type":"none"}),
                        ToolMode::Required(None) => serde_json::json!({"type":"any"}),
                        ToolMode::Required(Some(name)) => {
                            serde_json::json!({"type":"tool","name":name})
                        }
                    };
                }
                if let Some(v) = options.temperature {
                    body["temperature"] = serde_json::json!(v);
                }
                if let Some(v) = options.top_p {
                    body["top_p"] = serde_json::json!(v);
                }
                if let Some(v) = &options.stop {
                    body["stop_sequences"] = serde_json::json!(v);
                }
                Ok(body)
            }
            NativeProtocol::Gemini => {
                let mut body = serde_json::json!({
                  "contents": messages.iter().filter(|m| m.role.as_str() != "system").map(|m| Ok(serde_json::json!({"role": if m.role.as_str() == "assistant" { "model" } else { "user" }, "parts": content_json(m, false)?}))).collect::<agent_framework_core::error::Result<Vec<_>>>()?,
                  "generationConfig": {"maxOutputTokens": options.max_tokens, "temperature": options.temperature, "topP": options.top_p}
                });
                if let Some(system) = system {
                    body["systemInstruction"] = serde_json::json!({"parts":[{"text":system}]});
                }
                if !options.tools.is_empty() {
                    body["tools"] = serde_json::json!([{"functionDeclarations": options.tools.iter().map(|t| serde_json::json!({"name":t.name,"description":t.description,"parameters":t.parameters})).collect::<Vec<_>>() }]);
                }
                if let Some(choice) = &options.tool_choice {
                    body["toolConfig"] = serde_json::json!({"functionCallingConfig": match choice { ToolMode::Auto=>serde_json::json!({"mode":"AUTO"}), ToolMode::None=>serde_json::json!({"mode":"NONE"}), ToolMode::Required(None)=>serde_json::json!({"mode":"ANY"}), ToolMode::Required(Some(n))=>serde_json::json!({"mode":"ANY","allowedFunctionNames":[n]}) }});
                }
                Ok(body)
            }
        }
    }

    async fn post(
        &self,
        body: serde_json::Value,
        streaming: bool,
    ) -> agent_framework_core::error::Result<reqwest::Response> {
        use agent_framework_core::error::Error;
        let url = match (self.protocol, streaming) {
            (NativeProtocol::Anthropic, _) => {
                format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
            }
            (NativeProtocol::Gemini, false) => format!(
                "{}/models/{}:generateContent",
                self.base_url.trim_end_matches('/'),
                self.model
            ),
            (NativeProtocol::Gemini, true) => format!(
                "{}/models/{}:streamGenerateContent",
                self.base_url.trim_end_matches('/'),
                self.model
            ),
        };
        let mut body = body;
        if streaming && matches!(self.protocol, NativeProtocol::Anthropic) {
            body["stream"] = serde_json::json!(true);
        }
        let mut query_params = self.query_params.clone();
        if streaming && matches!(self.protocol, NativeProtocol::Gemini) {
            query_params.insert("alt".into(), "sse".into());
        }
        let mut headers = self.headers.clone();
        // A delegated token is re-resolved per request so an expiry mid-run
        // refreshes rather than replaying the token frozen in `headers`. The
        // fresh value REPLACES the construction-time one (a builder `header`
        // call would append a second, stale `Authorization`).
        let refreshed = self.refreshed_authorization().await?;
        if let Some(rendered) = &refreshed {
            let mut value = reqwest::header::HeaderValue::from_str(rendered)
                .map_err(|_| Error::service("delegated credential is not a valid header value"))?;
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        let response = self
            .http
            .post(url)
            .query(&query_params)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|error| native_transport_error("native provider request", &error))?;
        if !response.status().is_success() {
            let status = response.status();
            // Read the pacing hint before the body: `bounded_response` consumes
            // the response, and the headers go with it. Its OpenAI-compatible
            // sibling honors `Retry-After` and this one ignored it, so a native
            // provider's 429 was retried on the generic backoff schedule
            // instead of the wait the server actually asked for — hammering a
            // provider that just said "not yet".
            let retry_after_ms = retry_after_hint_ms(response.headers());
            let snippet =
                bounded_response(response, 1024, &self.secret_values(refreshed.as_deref())).await?;
            let mut message = format!("native provider API error {status}: {snippet}");
            if let Some(ms) = retry_after_ms {
                // The marker `codypendent_providers::retry` parses back out.
                message.push_str(&format!(" [retry-after-ms={ms}]"));
            }
            // The same classification the OpenAI-compatible path applies: an
            // Anthropic or Gemini 401 used to collapse to an untyped service
            // error, so the loop could neither name it as an authentication
            // failure nor decide its retryability the way it does for the
            // other transport.
            return Err(agent_framework_openai::classify_service_error(
                status.as_u16(),
                &snippet,
                message,
                None,
            ));
        }
        Ok(response)
    }

    /// Every value that must never reach a public error: the fixed headers and
    /// query params, the credential in both raw and rendered form, and — when a
    /// delegated token was refreshed for this request — the fresh value too,
    /// which by definition is not in the fixed header map.
    fn secret_values(&self, refreshed: Option<&str>) -> Vec<String> {
        self.headers
            .values()
            .filter_map(|value| value.to_str().ok().map(str::to_owned))
            .chain(self.query_params.values().cloned())
            .chain(self.credential_raw.iter().cloned())
            .chain(self.credential_rendered.iter().cloned())
            .chain(refreshed.into_iter().map(str::to_owned))
            .chain(
                refreshed
                    .and_then(|value| value.strip_prefix("Bearer "))
                    .map(str::to_owned),
            )
            .filter(|value| !value.is_empty())
            .collect()
    }

    fn normalize(
        &self,
        value: &serde_json::Value,
    ) -> agent_framework_core::error::Result<Vec<agent_framework_core::types::Content>> {
        use agent_framework_core::error::Error;
        use agent_framework_core::types::{Content, FunctionArguments, FunctionCallContent};
        if value.get("error").is_some()
            || value.get("type").and_then(|v| v.as_str()) == Some("error")
        {
            return Err(Error::service("native provider error event"));
        }
        match self.protocol {
            NativeProtocol::Anthropic => {
                if let Some(reason) = value.get("stop_reason").and_then(|v| v.as_str()) {
                    if !matches!(
                        reason,
                        "end_turn" | "max_tokens" | "tool_use" | "stop_sequence"
                    ) {
                        return Err(Error::service("Anthropic response ended abnormally"));
                    }
                }
            }
            NativeProtocol::Gemini => {
                if value.pointer("/promptFeedback/blockReason").is_some() {
                    return Err(Error::service("Gemini prompt was blocked"));
                }
                if let Some(reason) = value
                    .pointer("/candidates/0/finishReason")
                    .and_then(|v| v.as_str())
                {
                    if !matches!(reason, "STOP" | "MAX_TOKENS") {
                        return Err(Error::service("Gemini response ended abnormally"));
                    }
                }
            }
        }
        let parts = match self.protocol {
            NativeProtocol::Anthropic => value.get("content").and_then(|v| v.as_array()),
            NativeProtocol::Gemini => value
                .pointer("/candidates/0/content/parts")
                .and_then(|v| v.as_array()),
        }
        .ok_or_else(|| {
            Error::service("native provider response missing required content envelope")
        })?;
        let mut out = Vec::new();
        for p in parts {
            if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                out.push(Content::text(t));
            } else if self.protocol_is_anthropic()
                && p.get("type").and_then(|v| v.as_str()) == Some("tool_use")
            {
                out.push(Content::FunctionCall(FunctionCallContent::new(
                    required_str(p, "id")?,
                    required_str(p, "name")?,
                    Some(FunctionArguments::Object(
                        p.get("input")
                            .and_then(|v| v.as_object())
                            .ok_or_else(|| Error::service("malformed Anthropic tool_use"))?
                            .clone()
                            .into_iter()
                            .collect(),
                    )),
                )));
            } else if let Some(fc) = p.get("functionCall") {
                let args = fc
                    .get("args")
                    .and_then(|v| v.as_object())
                    .ok_or_else(|| Error::service("malformed Gemini functionCall"))?
                    .clone()
                    .into_iter()
                    .collect();
                out.push(Content::FunctionCall(FunctionCallContent::new(
                    fc.get("id")
                        .and_then(|v| v.as_str())
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(gemini_synthetic_call_id),
                    required_str(fc, "name")?,
                    Some(FunctionArguments::Object(args)),
                )));
            } else {
                return Err(Error::service("unsupported native provider response part"));
            }
        }
        if out.is_empty() {
            return Err(Error::service("native provider returned empty content"));
        }
        Ok(out)
    }
    fn protocol_is_anthropic(&self) -> bool {
        matches!(self.protocol, NativeProtocol::Anthropic)
    }
}

#[cfg(feature = "provider-openai")]
#[async_trait::async_trait]
impl agent_framework_core::client::ChatClient for NativeChatClient {
    async fn get_response(
        &self,
        messages: Vec<agent_framework_core::types::Message>,
        options: agent_framework_core::types::ChatOptions,
    ) -> agent_framework_core::error::Result<agent_framework_core::types::ChatResponse> {
        use agent_framework_core::error::Error;
        let response = self.post(self.body(&messages, &options)?, false).await?;
        let text = bounded_response(response, 1024 * 1024, &self.secret_values(None)).await?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| Error::service(format!("invalid native response json: {e}")))?;
        let contents = self.normalize(&value)?;
        Ok(agent_framework_core::types::ChatResponse {
            messages: vec![agent_framework_core::types::Message::with_contents(
                agent_framework_core::types::Role::assistant(),
                contents,
            )],
            // The provider's own token counts — the ONLY source the driver's
            // measured usage and routed cost are derived from.
            usage_details: NativeUsage::read(self.protocol, &value).map(NativeUsage::into_details),
            ..Default::default()
        })
    }

    async fn get_streaming_response(
        &self,
        messages: Vec<agent_framework_core::types::Message>,
        options: agent_framework_core::types::ChatOptions,
    ) -> agent_framework_core::error::Result<agent_framework_core::client::ChatStream> {
        use futures::StreamExt;
        let response = self.post(self.body(&messages, &options)?, true).await?;
        let protocol = self.protocol;
        let state = (
            response.bytes_stream(),
            SseDecoder::default(),
            StreamNormalizer::new(protocol),
            VecDeque::<
                agent_framework_core::error::Result<
                    agent_framework_core::types::ChatResponseUpdate,
                >,
            >::new(),
            false,
        );
        Ok(futures::stream::unfold(
            state,
            move |(mut bytes, mut decoder, mut normalizer, mut queue, mut eof)| async move {
                loop {
                    if let Some(item) = queue.pop_front() {
                        if item.is_err() {
                            eof = true;
                            queue.clear();
                        }
                        return Some((item, (bytes, decoder, normalizer, queue, eof)));
                    }
                    if eof {
                        return None;
                    }
                    match bytes.next().await {
                        Some(Ok(chunk)) => match decoder.push(&chunk, false) {
                            Ok(events) => {
                                for event in events {
                                    match normalizer.normalize(event) {
                                        Ok(Some(update)) => {
                                            queue.push_back(Ok(update));
                                            if normalizer.is_terminal() {
                                                eof = true;
                                                break;
                                            }
                                        }
                                        Ok(None) => {
                                            eof = true;
                                            break;
                                        }
                                        Err(error) => {
                                            queue.push_back(Err(error));
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                queue.push_back(Err(e));
                                eof = true;
                            }
                        },
                        Some(Err(error)) => {
                            queue.push_back(Err(native_transport_error("native stream", &error)));
                            eof = true;
                        }
                        None => {
                            eof = true;
                            match decoder.push(&[], true) {
                                Ok(events) => {
                                    for event in events {
                                        match normalizer.normalize(event) {
                                            Ok(Some(update)) => {
                                                queue.push_back(Ok(update));
                                                if normalizer.is_terminal() {
                                                    break;
                                                }
                                            }
                                            Ok(None) => break,
                                            Err(error) => {
                                                queue.push_back(Err(error));
                                                break;
                                            }
                                        }
                                    }
                                    if queue.iter().all(|item| item.is_ok())
                                        && !normalizer.is_terminal()
                                    {
                                        queue.push_back(Err(
                                            agent_framework_core::error::Error::service(
                                                "native stream ended before its protocol terminal event",
                                            ),
                                        ));
                                    }
                                }
                                Err(e) => queue.push_back(Err(e)),
                            }
                        }
                    }
                }
            },
        )
        .boxed())
    }
    fn model(&self) -> Option<&str> {
        Some(&self.model)
    }
}

#[cfg(feature = "provider-openai")]
fn required_str<'a>(
    value: &'a serde_json::Value,
    field: &str,
) -> agent_framework_core::error::Result<&'a str> {
    value.get(field).and_then(|v| v.as_str()).ok_or_else(|| {
        agent_framework_core::error::Error::service(format!("native response missing `{field}`"))
    })
}

#[cfg(feature = "provider-openai")]
async fn bounded_response(
    response: reqwest::Response,
    limit: usize,
    secrets: &[String],
) -> agent_framework_core::error::Result<String> {
    use futures::StreamExt;
    let max_secret_len = secrets.iter().map(String::len).max().unwrap_or(0);
    let read_limit = limit.saturating_add(max_secret_len);
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while bytes.len() <= read_limit {
        let Some(chunk) = stream.next().await else {
            break;
        };
        let chunk = chunk.map_err(|_| {
            agent_framework_core::error::Error::service("native response transport failed")
        })?;
        let take = read_limit
            .saturating_add(1)
            .saturating_sub(bytes.len())
            .min(chunk.len());
        bytes.extend_from_slice(&chunk[..take]);
        if bytes.len() > read_limit {
            break;
        }
    }
    let raw_truncated = bytes.len() > read_limit;
    bytes.truncate(read_limit);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if !raw_truncated && std::str::from_utf8(&bytes).is_err() {
        return Err(agent_framework_core::error::Error::service(
            "native response was not valid UTF-8",
        ));
    }
    let mut secrets = secrets.iter().filter(|s| !s.is_empty()).collect::<Vec<_>>();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    for secret in secrets {
        if !secret.is_empty() {
            text = text.replace(secret, "<redacted>");
        }
    }
    let truncated = raw_truncated || text.len() > limit;
    if text.len() > limit {
        let mut boundary = limit;
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
    }
    if truncated {
        text.push_str("… [truncated]");
    }
    Ok(text)
}

#[cfg(feature = "provider-openai")]
#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
    data: Vec<String>,
}
#[cfg(feature = "provider-openai")]
const MAX_SSE_PENDING_LINE_BYTES: usize = 64 * 1024;
#[cfg(feature = "provider-openai")]
const MAX_SSE_EVENT_DATA_BYTES: usize = 1024 * 1024;
#[cfg(feature = "provider-openai")]
const MAX_SSE_AGGREGATE_BYTES: usize = 2 * 1024 * 1024;
#[cfg(feature = "provider-openai")]
impl SseDecoder {
    fn push(
        &mut self,
        chunk: &[u8],
        eof: bool,
    ) -> agent_framework_core::error::Result<Vec<String>> {
        let event_bytes = self.data.iter().map(String::len).sum::<usize>();
        if self
            .buffer
            .len()
            .saturating_add(event_bytes)
            .saturating_add(chunk.len())
            > MAX_SSE_AGGREGATE_BYTES
        {
            return Err(agent_framework_core::error::Error::service(
                "native SSE aggregate data exceeded limit",
            ));
        }
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buffer.iter().position(|b| *b == b'\n') {
            let mut line = self.buffer.drain(..=pos).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(&line, &mut out)?;
        }
        if self.buffer.len() > MAX_SSE_PENDING_LINE_BYTES {
            return Err(agent_framework_core::error::Error::service(
                "native SSE line exceeded limit",
            ));
        }
        if eof {
            if !self.buffer.is_empty() {
                let line = std::mem::take(&mut self.buffer);
                self.line(&line, &mut out)?;
            }
            if !self.data.is_empty() {
                out.push(self.data.join("\n"));
                self.data.clear();
            }
        }
        Ok(out)
    }
    fn line(
        &mut self,
        line: &[u8],
        out: &mut Vec<String>,
    ) -> agent_framework_core::error::Result<()> {
        let line = std::str::from_utf8(line).map_err(|_| {
            agent_framework_core::error::Error::service("native SSE was not valid UTF-8")
        })?;
        if line.is_empty() {
            if !self.data.is_empty() {
                out.push(self.data.join("\n"));
                self.data.clear();
            }
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = line
            .split_once(':')
            .map(|(f, v)| (f, v.strip_prefix(' ').unwrap_or(v)))
            .unwrap_or((line, ""));
        match field {
            "data" => {
                let pending = self.data.iter().map(String::len).sum::<usize>()
                    + self.data.len().saturating_sub(1)
                    + value.len();
                if pending > MAX_SSE_EVENT_DATA_BYTES {
                    return Err(agent_framework_core::error::Error::service(
                        "native SSE event data exceeded limit",
                    ));
                }
                self.data.push(value.to_owned());
            }
            "event" | "id" | "retry" => {}
            _ => {}
        }
        Ok(())
    }
}

/// Describe a transport failure on the native (Anthropic / Gemini) path without
/// echoing it.
///
/// `reqwest::Error`'s `Display` includes the request URL, and Gemini's API key
/// rides in the query string, so the text must never be forwarded — which is why
/// this path used to collapse every failure to one fixed string. That string
/// matched nothing in the retry classifier, so a connect timeout, a reset or a
/// DNS blip failed the run at once while the OpenAI-compatible path (which does
/// format its error) retried. The *kind* is safe to name, so it is spelled with
/// the phrases the classifier recognises ("timed out", "connection error");
/// anything else stays the generic, non-retried message.
#[cfg(feature = "provider-openai")]
fn native_transport_error(
    what: &str,
    error: &reqwest::Error,
) -> agent_framework_core::error::Error {
    let message = if error.is_timeout() {
        format!("{what} timed out")
    } else if error.is_connect() {
        format!("{what} connection error")
    } else {
        format!("{what} failed")
    };
    agent_framework_core::error::Error::service(message)
}

#[cfg(feature = "provider-openai")]
struct StreamNormalizer {
    protocol: NativeProtocol,
    anthropic_tools: HashMap<u64, (String, String, bool)>,
    usage: UsageTally,
    terminal: bool,
}

#[cfg(feature = "provider-openai")]
impl StreamNormalizer {
    fn new(protocol: NativeProtocol) -> Self {
        Self {
            protocol,
            anthropic_tools: HashMap::new(),
            usage: UsageTally::default(),
            terminal: false,
        }
    }

    /// The `Content::Usage` items an event contributes: the increment its
    /// running totals added, or nothing when it carried no new counts.
    fn usage_contents(
        &mut self,
        envelope: &serde_json::Value,
    ) -> Vec<agent_framework_core::types::Content> {
        use agent_framework_core::types::{Content, UsageContent};
        NativeUsage::read(self.protocol, envelope)
            .and_then(|usage| self.usage.advance(usage))
            .map(|details| vec![Content::Usage(UsageContent { details })])
            .unwrap_or_default()
    }

    fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn normalize(
        &mut self,
        data: String,
    ) -> agent_framework_core::error::Result<Option<agent_framework_core::types::ChatResponseUpdate>>
    {
        use agent_framework_core::error::Error;
        use agent_framework_core::types::{
            ChatResponseUpdate, Content, FunctionArguments, FunctionCallContent,
        };
        if data == "[DONE]" {
            return if self.terminal {
                Ok(None)
            } else {
                Err(Error::service(
                    "native stream ended before its protocol terminal event",
                ))
            };
        }
        let value: serde_json::Value = serde_json::from_str(&data)
            .map_err(|e| Error::service(format!("malformed native SSE JSON: {e}")))?;
        if value.get("error").is_some()
            || value.get("type").and_then(|v| v.as_str()) == Some("error")
        {
            return Err(Error::service("native provider error event"));
        }
        let update = match self.protocol {
            NativeProtocol::Anthropic => match value.get("type").and_then(|v| v.as_str()) {
                Some("content_block_delta") => {
                    let delta = value
                        .get("delta")
                        .ok_or_else(|| Error::service("malformed Anthropic content_block_delta"))?;
                    match delta.get("type").and_then(|v| v.as_str()) {
                        Some("text_delta") => {
                            Ok(ChatResponseUpdate::text(required_str(delta, "text")?))
                        }
                        Some("input_json_delta") => {
                            let index =
                                value.get("index").and_then(|v| v.as_u64()).ok_or_else(|| {
                                    Error::service("Anthropic delta missing content-block index")
                                })?;
                            let (call_id, name, has_initial_input) =
                                self.anthropic_tools.get(&index).ok_or_else(|| {
                                    Error::service(
                                        "Anthropic delta referenced unknown content-block index",
                                    )
                                })?;
                            if *has_initial_input {
                                return Err(Error::service(
                                    "Anthropic tool input delta followed non-empty initial input",
                                ));
                            }
                            Ok(ChatResponseUpdate {
                                contents: vec![Content::FunctionCall(FunctionCallContent::new(
                                    call_id,
                                    name,
                                    Some(FunctionArguments::Raw(
                                        required_str(delta, "partial_json")?.to_owned(),
                                    )),
                                ))],
                                ..Default::default()
                            })
                        }
                        _ => Err(Error::service("malformed Anthropic delta shape")),
                    }
                }
                Some("content_block_start") => {
                    let block = value
                        .get("content_block")
                        .ok_or_else(|| Error::service("malformed Anthropic content_block_start"))?;
                    match block.get("type").and_then(|v| v.as_str()) {
                        Some("text") => Ok(ChatResponseUpdate::text(required_str(block, "text")?)),
                        Some("tool_use") => Ok(ChatResponseUpdate {
                            contents: vec![Content::FunctionCall(FunctionCallContent::new(
                                {
                                    let index = value
                                        .get("index")
                                        .and_then(|v| v.as_u64())
                                        .ok_or_else(|| {
                                            Error::service("Anthropic block start missing index")
                                        })?;
                                    let id = required_str(block, "id")?.to_owned();
                                    let name = required_str(block, "name")?.to_owned();
                                    let input = block
                                        .get("input")
                                        .and_then(|value| value.as_object())
                                        .ok_or_else(|| {
                                            Error::service("malformed Anthropic tool input")
                                        })?;
                                    let has_initial_input = !input.is_empty();
                                    if self
                                        .anthropic_tools
                                        .insert(index, (id.clone(), name, has_initial_input))
                                        .is_some()
                                    {
                                        return Err(Error::service(
                                            "duplicate Anthropic content-block index",
                                        ));
                                    }
                                    id
                                },
                                required_str(block, "name")?,
                                Some(
                                    if block
                                        .get("input")
                                        .and_then(|value| value.as_object())
                                        .is_some_and(|input| !input.is_empty())
                                    {
                                        FunctionArguments::Object(
                                            block["input"]
                                                .as_object()
                                                .expect("validated above")
                                                .clone()
                                                .into_iter()
                                                .collect(),
                                        )
                                    } else {
                                        FunctionArguments::Raw(String::new())
                                    },
                                ),
                            ))],
                            ..Default::default()
                        }),
                        _ => Err(Error::service("unsupported Anthropic content block")),
                    }
                }
                Some("message_delta") => {
                    if let Some(reason) =
                        value.pointer("/delta/stop_reason").and_then(|v| v.as_str())
                    {
                        if !matches!(
                            reason,
                            "end_turn" | "max_tokens" | "tool_use" | "stop_sequence"
                        ) {
                            return Err(Error::service("Anthropic stream ended abnormally"));
                        }
                    }
                    // `stop_reason: max_tokens` means the reply was CUT OFF, not
                    // finished. Surface it as the framework's `length` finish
                    // reason so the loop can tell a truncated tool call from a
                    // completed one instead of treating both as success.
                    let truncated = value.pointer("/delta/stop_reason").and_then(|v| v.as_str())
                        == Some("max_tokens");
                    // `message_delta` carries the message's final output count.
                    Ok(ChatResponseUpdate {
                        contents: self.usage_contents(&value),
                        finish_reason: truncated.then(|| {
                            agent_framework_core::types::FinishReason::new(
                                agent_framework_core::types::FinishReason::LENGTH,
                            )
                        }),
                        ..Default::default()
                    })
                }
                Some("message_stop") => {
                    self.terminal = true;
                    return Ok(None);
                }
                // `message_start` carries the prompt/cache counts, nested one
                // level down under the message it opens.
                Some("message_start") => Ok(ChatResponseUpdate {
                    contents: value
                        .get("message")
                        .map(|message| self.usage_contents(message))
                        .unwrap_or_default(),
                    ..Default::default()
                }),
                Some("content_block_stop" | "ping") => Ok(ChatResponseUpdate::default()),
                Some(_) | None => Err(Error::service("unknown or malformed Anthropic event shape")),
            },
            NativeProtocol::Gemini => {
                if value.pointer("/promptFeedback/blockReason").is_some() {
                    return Err(Error::service("Gemini stream was blocked"));
                }
                let mut truncated = false;
                if let Some(reason) = value
                    .pointer("/candidates/0/finishReason")
                    .and_then(|v| v.as_str())
                {
                    if !matches!(reason, "STOP" | "MAX_TOKENS") {
                        return Err(Error::service("Gemini stream ended abnormally"));
                    }
                    truncated = reason == "MAX_TOKENS";
                    self.terminal = true;
                }
                let parts = value
                    .pointer("/candidates/0/content/parts")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| Error::service("malformed Gemini stream candidate"))?;
                let mut contents = Vec::new();
                for p in parts {
                    if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                        contents.push(Content::text(t));
                    } else if let Some(fc) = p.get("functionCall") {
                        let args = fc
                            .get("args")
                            .and_then(|v| v.as_object())
                            .ok_or_else(|| Error::service("malformed Gemini functionCall"))?
                            .clone()
                            .into_iter()
                            .collect();
                        let call_id = fc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .filter(|id| !id.is_empty())
                            .map(str::to_owned)
                            .unwrap_or_else(gemini_synthetic_call_id);
                        contents.push(Content::FunctionCall(FunctionCallContent::new(
                            call_id,
                            required_str(fc, "name")?,
                            Some(FunctionArguments::Object(args)),
                        )));
                    } else {
                        return Err(Error::service("unsupported Gemini stream part"));
                    }
                }
                if contents.is_empty() {
                    return Err(Error::service("empty Gemini stream event"));
                }
                // `usageMetadata` repeats the message's running totals on every
                // chunk; only the increment is emitted.
                contents.extend(self.usage_contents(&value));
                Ok(ChatResponseUpdate {
                    contents,
                    finish_reason: truncated.then(|| {
                        agent_framework_core::types::FinishReason::new(
                            agent_framework_core::types::FinishReason::LENGTH,
                        )
                    }),
                    ..Default::default()
                })
            }
        }?;
        Ok(Some(update))
    }
}

/// An OpenAI-compatible chat client for providers whose auth is NOT the
/// framework default `Authorization: Bearer …` (Azure OpenAI's `api-key`
/// header) or that require provider-wide extra headers (GitHub Models'
/// `X-GitHub-Api-Version`). `OpenAIChatCompletionClient` hardcodes
/// `bearer_auth` with no header hook, so this client mirrors it
/// request-for-request by reusing the framework's own public conversion and
/// SSE-parsing helpers — only header injection differs. The auth header value
/// is marked sensitive so reqwest never echoes it into an error.
#[cfg(feature = "provider-openai")]
struct HeaderAuthChatClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    headers: reqwest::header::HeaderMap,
    query_params: BTreeMap<String, String>,
}

#[cfg(feature = "provider-openai")]
impl HeaderAuthChatClient {
    /// Build the client, assembling the fixed header set: the provider's
    /// extra headers plus (when a key is present) `header: prefix+key`,
    /// sensitive. `None` when a header name/value cannot be represented —
    /// the caller maps that to a legible config error naming no key.
    fn new(cfg: &ModelConfig, auth: &EndpointAuth, api_key: &str) -> Option<Self> {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut headers = HeaderMap::new();
        for (name, value) in &auth.extra_headers {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).ok()?,
                HeaderValue::from_str(value).ok()?,
            );
        }
        if !api_key.is_empty() {
            let mut value = HeaderValue::from_str(&format!("{}{api_key}", auth.prefix)).ok()?;
            value.set_sensitive(true);
            headers.insert(HeaderName::from_bytes(auth.header.as_bytes()).ok()?, value);
        }
        Some(Self {
            http: reqwest::Client::builder()
                .connect_timeout(PROVIDER_CONNECT_TIMEOUT)
                .read_timeout(PROVIDER_IDLE_TIMEOUT)
                .build()
                .ok()?,
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone(),
            headers,
            query_params: auth.query_params.clone(),
        })
    }

    /// The exact chat-completions body the stock client builds, via the
    /// framework's own public conversion helpers.
    fn build_body(
        &self,
        messages: &[agent_framework_core::types::Message],
        options: &agent_framework_core::types::ChatOptions,
        stream: bool,
    ) -> serde_json::Value {
        use agent_framework_openai::convert;
        let mut body = serde_json::Map::new();
        let model = options.model.clone().unwrap_or_else(|| self.model.clone());
        body.insert("model".into(), serde_json::json!(model));
        body.insert(
            "messages".into(),
            serde_json::json!(convert::messages_to_openai(messages)),
        );
        convert::apply_options(&mut body, options);
        let (tools, tool_choice) = convert::tools_to_openai(options);
        if let Some(tools) = tools {
            body.insert("tools".into(), tools);
        }
        if let Some(choice) = tool_choice {
            body.insert("tool_choice".into(), choice);
        }
        if stream {
            body.insert("stream".into(), serde_json::json!(true));
            body.insert(
                "stream_options".into(),
                serde_json::json!({ "include_usage": true }),
            );
        }
        serde_json::Value::Object(body)
    }

    async fn post(
        &self,
        body: &serde_json::Value,
    ) -> agent_framework_core::error::Result<reqwest::Response> {
        use agent_framework_core::error::Error;
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .query(&self.query_params)
            .headers(self.headers.clone())
            .json(body)
            .send()
            .await
            .map_err(|e| Error::service(format!("request failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let retry_after_ms = retry_after_hint_ms(resp.headers());
            let text = error_body(resp).await;
            let mut message = format!("OpenAI-compatible API error {status}: {text}");
            if let Some(ms) = retry_after_ms {
                message.push_str(&format!(" [retry-after-ms={ms}]"));
            }
            return Err(agent_framework_openai::classify_service_error(
                status.as_u16(),
                &text,
                message,
                None,
            ));
        }
        Ok(resp)
    }
}

/// The ceiling for any hint derived from a response header. A server header must
/// never be able to park a retry for days: `Retry-After: inf` saturates to
/// `u64::MAX` and a far-future HTTP date is effectively unbounded, so every
/// header-derived hint is clamped here (well under the retry module's own
/// `RETRY_MAX_DELAY_MS` ceiling — a few minutes is the most a header hint should
/// ever be honored for).
const RETRY_AFTER_HEADER_MAX_MS: u64 = 5 * 60 * 1000;

/// `retry-after-ms` (ms) wins over `retry-after` (integer/float seconds, or
/// an HTTP date relative to now). `None` when absent or unparseable. Every
/// header-derived hint is clamped to [`RETRY_AFTER_HEADER_MAX_MS`], and a
/// non-finite or negative `Retry-After` (e.g. `inf`, `-5`) is treated as no
/// hint — never a multi-day sleep and never an instant hot-retry.
fn retry_after_hint_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    if let Some(val) = headers.get("retry-after-ms") {
        if let Ok(s) = val.to_str() {
            if let Ok(ms) = s.trim().parse::<u64>() {
                return Some(ms.min(RETRY_AFTER_HEADER_MAX_MS));
            }
        }
    }
    if let Some(val) = headers.get(reqwest::header::RETRY_AFTER) {
        if let Ok(s) = val.to_str() {
            let s = s.trim();
            if let Ok(secs) = s.parse::<u64>() {
                return Some(secs.saturating_mul(1000).min(RETRY_AFTER_HEADER_MAX_MS));
            }
            if let Ok(secs_f) = s.parse::<f64>() {
                // Reject non-finite (`inf`/`nan`) and negative values: the first
                // saturates to `u64::MAX` (a ~24-day sleep past the no-hint cap),
                // the second casts to `0` (an instant hot-retry loop).
                if !secs_f.is_finite() || secs_f < 0.0 {
                    return None;
                }
                let ms = (secs_f * 1000.0).ceil() as u64;
                return Some(ms.min(RETRY_AFTER_HEADER_MAX_MS));
            }
            if let Ok(date) = chrono::DateTime::parse_from_rfc2822(s) {
                let now = chrono::Utc::now();
                let date_utc = date.with_timezone(&chrono::Utc);
                if let Ok(duration) = (date_utc - now).to_std() {
                    let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
                    return Some(ms.min(RETRY_AFTER_HEADER_MAX_MS));
                }
            }
        }
    }
    None
}

#[cfg(feature = "provider-openai")]
#[async_trait::async_trait]
impl agent_framework_core::client::ChatClient for HeaderAuthChatClient {
    async fn get_response(
        &self,
        messages: Vec<agent_framework_core::types::Message>,
        options: agent_framework_core::types::ChatOptions,
    ) -> agent_framework_core::error::Result<agent_framework_core::types::ChatResponse> {
        use agent_framework_core::error::Error;
        let body = self.build_body(&messages, &options, false);
        let resp = self.post(&body).await?;
        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::service(format!("invalid response json: {e}")))?;
        Ok(agent_framework_openai::convert::parse_response(&value))
    }

    async fn get_streaming_response(
        &self,
        messages: Vec<agent_framework_core::types::Message>,
        options: agent_framework_core::types::ChatOptions,
    ) -> agent_framework_core::error::Result<agent_framework_core::client::ChatStream> {
        use futures::StreamExt;
        let body = self.build_body(&messages, &options, true);
        let resp = self.post(&body).await?;
        Ok(agent_framework_openai::parse_sse_stream(resp).boxed())
    }

    fn model(&self) -> Option<&str> {
        Some(&self.model)
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
/// Reduce a `scheme://host[:port]/path...` base URL to the `host:port` a TCP
/// probe connects to, defaulting the port from the scheme. Public so the CLI's
/// local-endpoint probe names exactly the authority the runtime would try.
pub fn authority_from_base_url(base_url: &str) -> Result<String> {
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
        .is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|c| c.is_ascii_digit()));
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

/// Whether `base_url` names a MACHINE-LOCAL endpoint.
///
/// Compares the PARSED HOST exactly, never the URL text. A substring test on
/// the whole URL calls `https://localhost.example.com/v1` and
/// `https://models.example.com/localhost` local, and a caller that probes
/// local endpoints "for free" — because asking a server on this machine costs
/// nothing — would then send the configured authorization header to a remote
/// host the operator never permitted it to contact.
///
/// `0.0.0.0` and `::` count: a server bound to every interface is reached at
/// those addresses from this machine, and the pickers have always listed them.
#[must_use]
pub fn is_local_base_url(base_url: &str) -> bool {
    let Ok(authority) = authority_from_base_url(base_url) else {
        return false;
    };
    // `authority_from_base_url` always appends a port, so the host is
    // everything before the last colon; an IPv6 literal keeps its brackets.
    let host = authority
        .rsplit_once(':')
        .map_or(authority.as_str(), |(host, _)| host);
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.eq_ignore_ascii_case("localhost") {
        return true;
    }
    bare.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

/// The outcome of [`resolve_model`]: which candidate was selected, so the
/// caller (the agent loop) can attribute the run to this model id, per
/// STEP 1.9 / STEP 1.10 rule 3 ("every model request records: model id, ...").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub id: ModelId,
}

/// Walk `policy`'s candidates for `mode` in order, returning the first model
/// whose credentials resolve and whose provider `/models` catalog contains the
/// configured model name. This prevents an open Ollama port with a stale model
/// tag from being selected and failing the run immediately afterward.
///
/// See [`resolve_model_with_probe`] for the fallback semantics and for
/// injecting a different probe.
pub async fn resolve_model(
    registry: &ModelRegistry,
    policy: &ModelPolicy,
    mode: AgentMode,
) -> Result<ResolvedModel> {
    #[cfg(not(feature = "provider-openai"))]
    {
        return resolve_model_with_probe(registry, policy, mode, &TcpConnectProbe::default()).await;
    }

    #[cfg(feature = "provider-openai")]
    {
        let candidates = policy.candidates(mode);
        if candidates.is_empty() {
            return Err(ModelsError::NoCandidates { mode });
        }
        let mut attempts = Vec::with_capacity(candidates.len());
        for id in candidates {
            if registry.get(id).is_none() {
                attempts.push(CandidateFailure::unregistered(id.clone()));
                continue;
            }
            match registry.check_model(id).await {
                Ok(()) => return Ok(ResolvedModel { id: id.clone() }),
                Err(error) => attempts.push(CandidateFailure::of(id.clone(), &error)),
            }
        }
        Err(ModelsError::AllCandidatesFailed { mode, attempts })
    }
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
            attempts.push(CandidateFailure::unregistered(id.clone()));
            continue;
        };
        match probe.check(&cfg.base_url).await {
            Ok(()) => return Ok(ResolvedModel { id: id.clone() }),
            Err(error) => attempts.push(CandidateFailure::of(id.clone(), &error)),
        }
    }
    Err(ModelsError::AllCandidatesFailed { mode, attempts })
}

// ===========================================================================
// Audio: speech-to-text and text-to-speech (voice v1, rubric 8)
// ===========================================================================
//
// Both ride the SAME OpenAI-compatible plumbing the chat profiles use — the
// `/audio/transcriptions` and `/audio/speech` endpoints are served by Groq
// (`whisper-large-v3`, `whisper-large-v3-turbo`), OpenAI (`gpt-4o-transcribe`,
// `gpt-4o-mini-transcribe`, `tts-1`, `gpt-4o-mini-tts`), DeepInfra and Together,
// so voice needs no new provider protocol. Configuration is two OPTIONAL tables
// in the existing `models.toml`, parsed independently of `[[model]]` so a file
// without them is unchanged:
//
// ```toml
// [transcription]
// base_url = "https://api.groq.com/openai/v1"
// model = "whisper-large-v3-turbo"
// api_key_env = "GROQ_API_KEY"
// # local = true   # set for an on-device engine, so classified audio is
// #                # allowed to be transcribed under any policy ceiling
//
// [speech]
// base_url = "https://api.openai.com/v1"
// model = "gpt-4o-mini-tts"
// voice = "alloy"
// api_key_env = "OPENAI_API_KEY"
// ```
//
// NOTE ON VERIFICATION: the machine this was written on has NO audio hardware
// and no provider credentials. Every test below drives a wiremock server and
// fixture bytes. Nothing here is evidence that a real provider's response shape
// or a real audio device behaves as expected.

/// One `[transcription]` / `[speech]` table in `models.toml`.
///
/// Deliberately its own type rather than a reuse of [`ModelConfig`]: an audio
/// profile has no [`ModelId`] (there is exactly one of each, selected by the
/// table name, not by a policy candidate list) and TTS carries a `voice` that
/// means nothing to a chat profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioModelConfig {
    /// The OpenAI-compatible base URL, e.g. `https://api.groq.com/openai/v1`.
    /// The endpoint path (`/audio/transcriptions`, `/audio/speech`) is appended.
    pub base_url: String,
    /// The provider-side model name, e.g. `whisper-large-v3-turbo`.
    pub model: String,
    /// The NAME of the environment variable holding the API key, read at call
    /// time and never stored. Empty means no key (a local endpoint).
    #[serde(default)]
    pub api_key_env: String,
    /// TTS only: the provider's voice name, e.g. `alloy`. Ignored for STT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// TTS only: the requested container, e.g. `mp3`/`wav`/`opus`. Sent verbatim
    /// as `response_format`; omitted lets the provider choose its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Whether this endpoint runs ON THIS DEVICE (a local whisper.cpp server,
    /// an Ollama-hosted ASR). This is the single knob that decides whether the
    /// daemon's classification gate treats a transcription as
    /// [`TranscriptionMode::Local`](codypendent_protocol::input::TranscriptionMode)
    /// — under which even `Confidential` audio may be transcribed — or as
    /// `Remote`, which the operator's off-device ceiling governs. It defaults to
    /// `false`: an unmarked endpoint is assumed to leave the device, so the
    /// safe classification is the one you get by saying nothing.
    #[serde(default)]
    pub local: bool,
    /// Request timeout in seconds. Audio requests are slower than chat ones (a
    /// minute of speech is a megabyte-scale upload), hence the generous default.
    #[serde(default = "default_audio_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_audio_timeout_secs() -> u64 {
    120
}

/// The optional audio tables in `models.toml`. Parsed with its OWN struct
/// (rather than by extending the `[[model]]` file shape) so an existing
/// `models.toml` — and every existing reader of it — is entirely unaffected.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AudioModels {
    /// The `[transcription]` table: the speech-to-text endpoint.
    #[serde(default)]
    pub transcription: Option<AudioModelConfig>,
    /// The `[speech]` table: the text-to-speech endpoint.
    #[serde(default)]
    pub speech: Option<AudioModelConfig>,
}

/// Parse the `[transcription]` / `[speech]` tables from `models.toml`.
///
/// A missing FILE yields an empty [`AudioModels`] (voice is simply not
/// configured — the overwhelmingly common case), but a file that exists and
/// does not parse is an error, so a typo in a voice table surfaces instead of
/// silently disabling voice.
pub fn load_audio_models(path: &Path) -> Result<AudioModels> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AudioModels::default())
        }
        Err(source) => {
            return Err(ModelsError::ReadConfig {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    toml::from_str(&text).map_err(|source| ModelsError::ParseConfig {
        path: path.to_path_buf(),
        source,
    })
}

/// Errors from the audio clients and the playback command.
///
/// Its own enum rather than new [`ModelsError`] variants: the audio path has
/// failure modes (no player configured, a spawn failure) that have nothing to
/// do with chat model resolution, and keeping them separate means a caller can
/// exhaustively match one without the other.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AudioError {
    /// `models.toml` has no `[transcription]` / `[speech]` table.
    #[error("no [{table}] entry in models.toml; voice {feature} is not configured")]
    NotConfigured {
        /// The missing table name.
        table: &'static str,
        /// What it would have enabled, for the message.
        feature: &'static str,
    },
    /// The configured `api_key_env` names an unset environment variable.
    #[error("environment variable {var} is not set (needed by the [{table}] entry)")]
    MissingApiKeyEnv {
        /// The table whose `api_key_env` could not be resolved.
        table: &'static str,
        /// The environment variable NAME (never a value).
        var: String,
    },
    /// The request could not be sent (DNS, TLS, connect, timeout).
    #[error("audio request to {url} failed: {source}")]
    Transport {
        /// The endpoint that was called.
        url: String,
        /// The underlying transport error.
        source: reqwest::Error,
    },
    /// The provider answered with a non-success status.
    #[error("audio endpoint {url} returned {status}: {body}")]
    Status {
        /// The endpoint that was called.
        url: String,
        /// The HTTP status.
        status: u16,
        /// The (truncated) response body, for diagnosis.
        body: String,
    },
    /// The provider's response did not have the documented shape.
    #[error("audio endpoint {url} returned an unreadable response: {detail}")]
    MalformedResponse {
        /// The endpoint that was called.
        url: String,
        /// What was wrong.
        detail: String,
    },
    /// Playback was requested with no `play_command` configured.
    #[error("no voice play_command is configured; set voice.play_command in config.toml (e.g. [\"mpv\", \"--no-terminal\", \"-\"])")]
    NoPlayer,
    /// The playback command could not be started or fed.
    #[error("could not run the playback command {command:?}: {source}")]
    Playback {
        /// The configured command, for the message.
        command: Vec<String>,
        /// The spawn/write failure.
        source: std::io::Error,
    },
}

/// Join a base URL and an endpoint path without doubling or dropping the slash.
fn audio_url(base_url: &str, path: &str) -> String {
    format!("{}/{}", base_url.trim_end_matches('/'), path)
}

/// Resolve an audio profile's API key with the SAME precedence a chat profile
/// uses: an `auth.json` entry first (keyed by the table name, so a TUI-saved key
/// applies), then the configured environment variable via the shared
/// [`credential_for`](codypendent_providers::credential_for) seam. An empty
/// `api_key_env` means "no key needed" and resolves to an empty string.
async fn audio_api_key(
    config: &AudioModelConfig,
    auth: &AuthStore,
    table: &'static str,
) -> std::result::Result<String, AudioError> {
    if let Some(key) = auth.get(table).filter(|key| !key.is_empty()) {
        return Ok(key.to_string());
    }
    if config.api_key_env.trim().is_empty() {
        return Ok(String::new());
    }
    let method = codypendent_providers::AuthMethod::ApiKey {
        env: vec![config.api_key_env.clone()],
        header: "Authorization".to_string(),
        prefix: "Bearer ".to_string(),
    };
    match codypendent_providers::credential_for(&method)
        .resolve()
        .await
    {
        Ok(codypendent_providers::ResolvedCredential::ApiKey { value, .. }) => Ok(value),
        Ok(codypendent_providers::ResolvedCredential::BearerToken { value, .. }) => Ok(value),
        Ok(codypendent_providers::ResolvedCredential::None) => Ok(String::new()),
        Err(codypendent_providers::CredentialError::MissingEnv { var }) => {
            Err(AudioError::MissingApiKeyEnv { table, var })
        }
        Err(other) => Err(AudioError::MissingApiKeyEnv {
            table,
            var: other.to_string(),
        }),
    }
}

/// Read a response body as text, truncated so a provider's HTML error page
/// cannot flood a log line or an error message.
///
/// The cut is walked back to a UTF-8 character boundary: the body is
/// provider-controlled, so a byte-index `String::truncate` **panics** the moment
/// a multi-byte character straddles the limit (a non-ASCII error page is all it
/// takes). Same shape as the native-transport reader's boundary walk above.
async fn error_body(response: reqwest::Response) -> String {
    clamp_error_body(response.text().await.unwrap_or_default())
}

/// How much of a provider error body is kept by [`error_body`].
const ERROR_BODY_LIMIT: usize = 512;

/// Clamp a provider-controlled body to [`ERROR_BODY_LIMIT`] bytes, cutting only
/// on a character boundary.
fn clamp_error_body(mut body: String) -> String {
    if body.len() > ERROR_BODY_LIMIT {
        let mut boundary = ERROR_BODY_LIMIT;
        while !body.is_char_boundary(boundary) {
            boundary -= 1;
        }
        body.truncate(boundary);
    }
    body
}

/// A speech-to-text client over an OpenAI-compatible
/// `{base_url}/audio/transcriptions` endpoint.
///
/// The request is `multipart/form-data` with a `file` part (the audio bytes)
/// and a `model` part, exactly as Groq/OpenAI/DeepInfra/Together document. The
/// multipart body is assembled by hand rather than through `reqwest`'s
/// `multipart` feature, so voice adds no new feature flag to the workspace's
/// shared HTTP dependency; the boundary is derived from the payload's own
/// SHA-256, which cannot collide with content it is a digest of.
#[derive(Debug, Clone)]
pub struct AudioTranscriber {
    config: AudioModelConfig,
    auth: AuthStore,
    http: reqwest::Client,
}

impl AudioTranscriber {
    /// Build a transcriber from the `[transcription]` table, or
    /// [`AudioError::NotConfigured`] when there is none.
    pub fn new(models: &AudioModels, auth: AuthStore) -> std::result::Result<Self, AudioError> {
        let config = models
            .transcription
            .clone()
            .ok_or(AudioError::NotConfigured {
                table: "transcription",
                feature: "input (speech-to-text)",
            })?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .unwrap_or_default();
        Ok(Self { config, auth, http })
    }

    /// The configured profile (endpoint, model name, locality).
    #[must_use]
    pub fn config(&self) -> &AudioModelConfig {
        &self.config
    }

    /// Whether this endpoint runs on-device — what the daemon's classification
    /// gate reads to pick `Local` vs `Remote`.
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.config.local
    }

    /// Transcribe `bytes` (an audio container named `filename`, whose extension
    /// is how most providers detect the format) and return the recognized text.
    pub async fn transcribe(
        &self,
        bytes: &[u8],
        filename: &str,
        media_type: &str,
    ) -> std::result::Result<String, AudioError> {
        let url = audio_url(&self.config.base_url, "audio/transcriptions");
        let key = audio_api_key(&self.config, &self.auth, "transcription").await?;
        let boundary = multipart_boundary(bytes);
        let body = multipart_body(&boundary, bytes, filename, media_type, &self.config.model);

        let mut request = self
            .http
            .post(&url)
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body);
        if !key.is_empty() {
            request = request.header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"));
        }
        let response = request
            .send()
            .await
            .map_err(|source| AudioError::Transport {
                url: url.clone(),
                source,
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(AudioError::Status {
                url,
                status: status.as_u16(),
                body: error_body(response).await,
            });
        }
        let payload: serde_json::Value =
            response
                .json()
                .await
                .map_err(|source| AudioError::MalformedResponse {
                    url: url.clone(),
                    detail: source.to_string(),
                })?;
        payload
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or(AudioError::MalformedResponse {
                url,
                detail: "no string `text` field in the response".to_string(),
            })
    }
}

/// A boundary derived from the payload's SHA-256. A multipart boundary must not
/// occur inside any part; a digest of the very bytes it delimits cannot, short
/// of a preimage the payload would have to contain of itself.
fn multipart_boundary(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("codypendentaudio{}", hex::encode(&hasher.finalize()[..12]))
}

/// Assemble the `multipart/form-data` body: a `file` part then a `model` part.
fn multipart_body(
    boundary: &str,
    bytes: &[u8],
    filename: &str,
    media_type: &str,
    model: &str,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + 512);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {media_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"model\"\r\n\r\n");
    body.extend_from_slice(model.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// Audio a [`AudioSynthesizer`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynthesizedSpeech {
    /// The encoded audio bytes, in whatever container the provider returned.
    pub bytes: Vec<u8>,
    /// The `Content-Type` the provider reported, e.g. `audio/mpeg`.
    pub media_type: String,
}

/// A text-to-speech client over an OpenAI-compatible `{base_url}/audio/speech`
/// endpoint (Groq, OpenAI, DeepInfra, Together). The reply is raw audio bytes,
/// not JSON.
#[derive(Debug, Clone)]
pub struct AudioSynthesizer {
    config: AudioModelConfig,
    auth: AuthStore,
    http: reqwest::Client,
}

impl AudioSynthesizer {
    /// Build a synthesizer from the `[speech]` table, or
    /// [`AudioError::NotConfigured`] when there is none.
    pub fn new(models: &AudioModels, auth: AuthStore) -> std::result::Result<Self, AudioError> {
        let config = models.speech.clone().ok_or(AudioError::NotConfigured {
            table: "speech",
            feature: "output (text-to-speech)",
        })?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .unwrap_or_default();
        Ok(Self { config, auth, http })
    }

    /// The configured profile.
    #[must_use]
    pub fn config(&self) -> &AudioModelConfig {
        &self.config
    }

    /// Synthesize `text` into audio bytes.
    pub async fn synthesize(
        &self,
        text: &str,
    ) -> std::result::Result<SynthesizedSpeech, AudioError> {
        let url = audio_url(&self.config.base_url, "audio/speech");
        let key = audio_api_key(&self.config, &self.auth, "speech").await?;
        let mut payload = serde_json::json!({
            "model": self.config.model,
            "input": text,
        });
        if let Some(voice) = &self.config.voice {
            payload["voice"] = serde_json::Value::String(voice.clone());
        }
        if let Some(format) = &self.config.format {
            payload["response_format"] = serde_json::Value::String(format.clone());
        }

        let mut request = self.http.post(&url).json(&payload);
        if !key.is_empty() {
            request = request.header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"));
        }
        let response = request
            .send()
            .await
            .map_err(|source| AudioError::Transport {
                url: url.clone(),
                source,
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(AudioError::Status {
                url,
                status: status.as_u16(),
                body: error_body(response).await,
            });
        }
        let media_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("audio/mpeg")
            .to_string();
        let bytes = response
            .bytes()
            .await
            .map_err(|source| AudioError::Transport {
                url: url.clone(),
                source,
            })?
            .to_vec();
        if bytes.is_empty() {
            return Err(AudioError::MalformedResponse {
                url,
                detail: "the speech endpoint returned no audio bytes".to_string(),
            });
        }
        Ok(SynthesizedSpeech { bytes, media_type })
    }
}

/// Plays synthesized audio by piping it to a USER-CONFIGURED command's stdin.
///
/// There is no bundled audio backend on purpose: shipping one would mean a
/// platform-specific native dependency for a strictly optional feature, and
/// every desktop already has a player that reads stdin (`mpv --no-terminal -`,
/// `ffplay -nodisp -autoexit -`, `paplay`, `afplay` via a temp file). The
/// operator names theirs in `config.toml`.
///
/// The child is spawned **detached**: its handle is dropped immediately, so a
/// long clip never blocks the caller's task, and its stdout/stderr are silenced
/// so a chatty player cannot corrupt a TUI's terminal. Nothing here has been
/// exercised against a real audio device — this container has none; the tests
/// substitute a `cat > file` "player" and assert the bytes arrive on its stdin.
#[derive(Debug, Clone, Default)]
pub struct AudioPlayer {
    command: Vec<String>,
}

impl AudioPlayer {
    /// A player driven by `command` (program plus arguments). An empty command
    /// means "unconfigured": [`play`](Self::play) then fails
    /// [`AudioError::NoPlayer`] instead of guessing a binary.
    #[must_use]
    pub fn new(command: Vec<String>) -> Self {
        Self { command }
    }

    /// Whether a playback command is configured.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.command.is_empty()
    }

    /// Pipe `bytes` to the configured command's stdin and return without
    /// waiting for it to finish.
    ///
    /// The write itself is awaited (so a spawn/pipe failure is reported rather
    /// than silently swallowed) but the process is NOT waited on. A player that
    /// outlives the caller is intentional: playback continues while the UI
    /// carries on. Note this leaves the child unreaped by this process; callers
    /// that spawn many should run this from a task whose runtime reaps children
    /// (tokio's `Command` does so via its signal handling).
    pub async fn play(&self, bytes: &[u8]) -> std::result::Result<(), AudioError> {
        use tokio::io::AsyncWriteExt as _;

        let Some((program, args)) = self.command.split_first() else {
            return Err(AudioError::NoPlayer);
        };
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(false)
            .spawn()
            .map_err(|source| AudioError::Playback {
                command: self.command.clone(),
                source,
            })?;
        let mut stdin = child.stdin.take().ok_or_else(|| AudioError::Playback {
            command: self.command.clone(),
            source: std::io::Error::other("the playback command has no stdin"),
        })?;
        stdin
            .write_all(bytes)
            .await
            .map_err(|source| AudioError::Playback {
                command: self.command.clone(),
                source,
            })?;
        // Closing stdin is what tells the player the clip is complete.
        stdin.shutdown().await.ok();
        drop(stdin);
        // Detach: the clip plays on while this task returns immediately.
        drop(child);
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    /// The two classifiers must agree about the same error.
    ///
    /// `failure_class` reads `ModelUnavailable`'s provider-supplied reason and
    /// calls a 429 or 5xx transient; `classify_models_error` used to hardcode
    /// `retryable: false` for it, so a client reading the wire was told a
    /// recoverable provider hiccup was permanent — and pointed at model
    /// configuration, which had nothing to do with it.
    #[test]
    fn wire_retryability_agrees_with_the_failure_class() {
        let cases = vec![
            ModelsError::ModelUnavailable {
                model: ModelId("m".to_owned()),
                provider_model: "pm".to_owned(),
                reason: "HTTP 429 Too Many Requests".to_owned(),
            },
            ModelsError::ModelUnavailable {
                model: ModelId("m".to_owned()),
                provider_model: "pm".to_owned(),
                reason: "HTTP 503 Service Unavailable".to_owned(),
            },
            ModelsError::ModelUnavailable {
                model: ModelId("m".to_owned()),
                provider_model: "pm".to_owned(),
                reason: "no such model in this account".to_owned(),
            },
            ModelsError::ConnectionFailed {
                base_url: "http://127.0.0.1:1/v1".to_owned(),
                reason: "connection refused".to_owned(),
            },
            ModelsError::MissingApiKeyEnv {
                model: ModelId("m".to_owned()),
                var: "SOME_KEY".to_owned(),
            },
            ModelsError::UnknownModel(ModelId("m".to_owned())),
        ];
        for error in &cases {
            let transient = matches!(error.failure_class(), FailureClass::Transient);
            let wire = classify_models_error(error);
            assert_eq!(
                wire.retryable, transient,
                "the wire and the failure class disagree about {error:?}: \
                 retryable={} transient={transient}",
                wire.retryable
            );
        }
    }

    /// A busy provider is not a misconfigured model: telling somebody to go and
    /// reconfigure one is advice that cannot help, and hides the retry that can.
    #[test]
    fn a_transient_model_unavailable_offers_retry_not_reconfigure() {
        use codypendent_protocol::UserAction;

        let busy = ModelsError::ModelUnavailable {
            model: ModelId("m".to_owned()),
            provider_model: "pm".to_owned(),
            reason: "HTTP 503 from /models".to_owned(),
        };
        let wire = classify_models_error(&busy);
        assert!(wire.retryable, "a 503 is retryable");
        assert_eq!(wire.user_action, Some(UserAction::Retry));

        let gone = ModelsError::ModelUnavailable {
            model: ModelId("m".to_owned()),
            provider_model: "pm".to_owned(),
            reason: "model has been retired".to_owned(),
        };
        let wire = classify_models_error(&gone);
        assert!(!wire.retryable, "a retired model is not retryable");
        assert_eq!(wire.user_action, Some(UserAction::ReconfigureModel));
    }

    use super::*;

    /// "Local" is a property of the HOST, not of the URL text. Searching the
    /// whole URL for "localhost" calls a remote host local, and a caller that
    /// probes local endpoints for free would then send a configured
    /// credential to it.
    #[test]
    fn local_is_decided_by_the_parsed_host_never_by_the_url_text() {
        for local in [
            "http://localhost:11434/v1",
            "http://LOCALHOST:1234/v1",
            "http://127.0.0.1:1234/v1",
            "http://127.0.0.2:8000/v1",
            "http://[::1]:8000/v1",
            "http://0.0.0.0:8000/v1",
            "http://localhost/v1",
        ] {
            assert!(is_local_base_url(local), "{local} is on this machine");
        }
        for remote in [
            // The reported shapes: the text contains "localhost", the host
            // does not.
            "https://localhost.example.com/v1",
            "https://models.example.com/localhost",
            "https://api.openai.com/v1",
            "https://127.0.0.1.example.com/v1",
            "",
        ] {
            assert!(
                !is_local_base_url(remote),
                "{remote} is NOT on this machine"
            );
        }
    }

    /// Every candidate failing the SAME way is that way, not a generic
    /// "reconfigure the model": four models all missing their key must send a
    /// person to API Keys. Candidates that disagree get no `user_action` at
    /// all, so the client falls back to reading the message rather than being
    /// pointed somewhere only one of them justified.
    #[test]
    fn an_aggregate_failure_speaks_only_for_what_its_candidates_agree_on() {
        use codypendent_protocol::UserAction;

        let missing = |id: &str, var: &str| {
            CandidateFailure::of(
                ModelId(id.to_string()),
                &ModelsError::MissingApiKeyEnv {
                    model: ModelId(id.to_string()),
                    var: var.to_string(),
                },
            )
        };
        let unreachable = |id: &str| {
            CandidateFailure::of(
                ModelId(id.to_string()),
                &ModelsError::ConnectionFailed {
                    base_url: "http://localhost:11434/v1".to_string(),
                    reason: "connection refused".to_string(),
                },
            )
        };

        // Unanimous: the credential is the answer, and it is not retryable.
        let all_missing = ModelsError::AllCandidatesFailed {
            mode: AgentMode::Build,
            attempts: vec![
                missing("a", "OPENAI_API_KEY"),
                missing("b", "ANTHROPIC_API_KEY"),
            ],
        };
        let classified = classify_run_failure(&anyhow::Error::from(all_missing)).expect("typed");
        assert_eq!(classified.user_action, Some(UserAction::Reauthenticate));
        assert_eq!(classified.code, "model.missing-credential");
        assert!(!classified.retryable);

        // Unanimous and transient: every endpoint refused the connection.
        let all_unreachable = ModelsError::AllCandidatesFailed {
            mode: AgentMode::Build,
            attempts: vec![unreachable("a"), unreachable("b")],
        };
        let classified =
            classify_run_failure(&anyhow::Error::from(all_unreachable)).expect("typed");
        assert_eq!(classified.user_action, Some(UserAction::ReconfigureModel));
        assert!(classified.retryable, "every candidate was retryable");

        // Divided: one missing key, one unreachable endpoint. No single next
        // step is right, so none is claimed and the client reads the text.
        let mixed = ModelsError::AllCandidatesFailed {
            mode: AgentMode::Build,
            attempts: vec![missing("a", "OPENAI_API_KEY"), unreachable("b")],
        };
        let classified = classify_run_failure(&anyhow::Error::from(mixed)).expect("typed");
        assert_eq!(
            classified.user_action, None,
            "a divided list must not point anywhere in particular"
        );
        assert_eq!(classified.code, "model.all-candidates-failed");
        assert!(
            classified.retryable,
            "an identical retry re-walks the whole list, and the unreachable \
             endpoint may be back by then"
        );
        assert!(
            classified.message.contains("OPENAI_API_KEY"),
            "the message still names every cause: {}",
            classified.message
        );
    }

    /// The structured half of a failed run comes off the TYPED cause in the
    /// chain — the framework's auth error under any amount of `context`, or
    /// this crate's own `ModelsError` — and is absent when nothing typed is
    /// there, so a client only ever sees an affordance the daemon vouched for.
    #[test]
    fn a_run_failure_is_classified_from_its_typed_cause() {
        use codypendent_protocol::UserAction;

        let auth: anyhow::Error =
            anyhow::Error::from(agent_framework_core::Error::ServiceInvalidAuth {
                message: "invalid x-api-key".to_string(),
            })
            .context("model stream failed");
        let classified = classify_run_failure(&auth).expect("typed cause");
        assert_eq!(classified.code, "model.invalid-auth");
        assert_eq!(classified.user_action, Some(UserAction::Reauthenticate));
        assert!(!classified.retryable);
        assert!(classified.message.contains("invalid x-api-key"));

        let overloaded = anyhow::Error::from(agent_framework_core::Error::ServiceStatus {
            status: 529,
            message: "overloaded".to_string(),
            retry_after: None,
        });
        let classified = classify_run_failure(&overloaded).expect("typed cause");
        assert_eq!(classified.code, "model.unavailable");
        assert_eq!(classified.user_action, Some(UserAction::Retry));
        assert!(classified.retryable);

        let unreachable = anyhow::Error::from(ModelsError::ConnectionFailed {
            base_url: "http://localhost:11434/v1".to_string(),
            reason: "connection refused".to_string(),
        })
        .context("model driver error");
        let classified = classify_run_failure(&unreachable).expect("typed cause");
        assert_eq!(classified.code, "model.unreachable");
        assert_eq!(classified.user_action, Some(UserAction::ReconfigureModel));

        assert!(
            classify_run_failure(&anyhow::anyhow!("something else entirely")).is_none(),
            "an untyped failure carries no structured half"
        );
    }
    use std::io::Write;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn model_id(s: &str) -> ModelId {
        ModelId(s.to_string())
    }

    /// A provider error body is attacker/provider-controlled text. Cutting it at
    /// a fixed BYTE index panics whenever a multi-byte character straddles the
    /// limit — `String::truncate` requires a char boundary — so an ordinary
    /// non-ASCII error page took the process down. Reverting the boundary walk
    /// makes this test panic rather than fail.
    #[test]
    fn error_body_truncation_never_splits_a_character() {
        // 170 three-byte characters = 510 bytes, then one more three-byte
        // character spanning bytes 510..513: byte 512 is mid-character.
        let body = "€".repeat(171);
        assert!(!body.is_char_boundary(ERROR_BODY_LIMIT));

        let clamped = clamp_error_body(body);
        assert_eq!(clamped.len(), 510, "cut back to the last whole character");
        assert_eq!(clamped.chars().count(), 170);

        // ASCII (every index a boundary) still cuts exactly at the limit, and a
        // short body is returned whole.
        assert_eq!(clamp_error_body("a".repeat(600)).len(), ERROR_BODY_LIMIT);
        assert_eq!(clamp_error_body("short".to_string()), "short");
    }

    #[test]
    fn retry_after_header_rejects_nonfinite_and_negative_and_clamps() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

        // `inf` previously saturated to u64::MAX (a ~24-day sleep past the
        // no-hint cap); it must now be treated as no hint.
        let mut inf = HeaderMap::new();
        inf.insert(RETRY_AFTER, HeaderValue::from_static("inf"));
        assert_eq!(retry_after_hint_ms(&inf), None);

        // A negative value previously cast to 0 (an instant hot-retry loop).
        let mut neg = HeaderMap::new();
        neg.insert(RETRY_AFTER, HeaderValue::from_static("-5"));
        assert_eq!(retry_after_hint_ms(&neg), None);

        // A normal small value still works, converted to milliseconds.
        let mut ok = HeaderMap::new();
        ok.insert(RETRY_AFTER, HeaderValue::from_static("2"));
        assert_eq!(retry_after_hint_ms(&ok), Some(2_000));

        // A huge but finite value is clamped to the header ceiling, never honored
        // as a multi-day sleep.
        let mut huge = HeaderMap::new();
        huge.insert(RETRY_AFTER, HeaderValue::from_static("100000"));
        assert_eq!(retry_after_hint_ms(&huge), Some(RETRY_AFTER_HEADER_MAX_MS));
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

        // Neither entry sets `context_tokens` — both must default to `None`
        // (back-compatible: an existing models.toml with no such key parses
        // unchanged), never a fabricated window.
        assert_eq!(configs[0].context_tokens, None);
        assert_eq!(configs[1].context_tokens, None);

        // ModelRegistry::load goes through the same path and should agree.
        let registry = ModelRegistry::load(file.path()).expect("load registry");
        assert!(registry.get(&model_id("hosted-default")).is_some());
        assert!(registry.get(&model_id("local-default")).is_some());
        assert_eq!(registry.ids().count(), 2);
    }

    #[tokio::test]
    async fn acp_profile_is_selectable_but_not_exposed_as_a_chat_client() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("models.toml");
        std::fs::write(
            &path,
            r#"[[model]]
id = "acp/codex-acp"
provider = "acp"
model = "codex-acp"
"#,
        )
        .expect("write minimal ACP config");
        let parsed = load_models(&path).expect("ACP does not need base_url/api_key_env");
        assert_eq!(parsed[0].base_url, "");
        assert_eq!(parsed[0].api_key_env, "");

        let config = ModelConfig {
            id: model_id("acp/codex-acp"),
            provider: "acp".to_string(),
            base_url: String::new(),
            model: "codex-acp".to_string(),
            api_key_env: String::new(),
            context_tokens: None,
            provider_id: None,
        };
        let registry = ModelRegistry::new(vec![config]);
        assert!(registry.is_acp(&model_id("acp/codex-acp")));
        assert_eq!(
            registry.acp_agent_id(&model_id("acp/codex-acp")),
            Some("codex-acp")
        );
        registry
            .check_model(&model_id("acp/codex-acp"))
            .await
            .expect("assembly executor owns ACP readiness");
        assert!(matches!(
            registry.client_for(&model_id("acp/codex-acp")).await,
            Err(ModelsError::ProtocolNotWired { .. })
        ));
    }

    #[test]
    fn context_tokens_parses_when_present_and_defaults_to_none_when_absent() {
        // Context-window protection (BT1): `context_tokens` is additive and
        // optional. A user who sets it under a `[[model]]` entry gets it back
        // as `Some(n)`; a config that omits it (the existing/back-compat
        // shape) must still parse, defaulting to `None` — never a fabricated
        // window.
        let mut file = tempfile::NamedTempFile::new().expect("create temp file");
        write!(
            file,
            r#"
[[model]]
id = "local-default"
provider = "openai-compatible"
base_url = "http://localhost:11434/v1"
model = "qwen2.5-coder:14b"
api_key_env = ""
context_tokens = 32768

[[model]]
id = "hosted-default"
provider = "openai-compatible"
base_url = "https://api.openai.com/v1"
model = "gpt-5.1-codex"
api_key_env = "OPENAI_API_KEY"
"#
        )
        .expect("write temp file");

        let configs = load_models(file.path()).expect("parse models.toml");
        assert_eq!(configs.len(), 2);
        assert_eq!(
            configs[0].context_tokens,
            Some(32_768),
            "an explicit context_tokens must parse verbatim"
        );
        assert_eq!(
            configs[1].context_tokens, None,
            "an entry with no context_tokens key must default to None, not a fabricated value"
        );
    }

    /// F4 (`2026-08-13-verticals/acp-models.md`): `crates/cli/src/tui.rs`'s
    /// `merge_catalog_rows` lets a provider's own `/models` response win over
    /// the curated catalog's `context_tokens` with no upper-bound check
    /// (`context_tokens.filter(|tokens| *tokens > 0)` is the only validation),
    /// and that value is persisted into `models.toml` verbatim, then forwarded
    /// as Ollama's `num_ctx` request hint and used as the TUI footer's
    /// context-usage denominator — "a misconfigured or hostile OpenAI-compatible
    /// gateway that reports `"context_length": 18446744073709551615` gets that
    /// number sent back as `num_ctx`". (The literal `u64::MAX` cannot itself
    /// ride through a `models.toml` round trip — TOML integers are signed
    /// 64-bit, so a provider's `serde_json` `u64::MAX` fails to serialize as a
    /// TOML integer at write time, before this clamp is even reached; this
    /// test uses a value that DOES survive the round trip — still absurd for
    /// any real model, still well under `i64::MAX` — to pin the clamp for
    /// every value that actually reaches parsing.) `load_models` is the one
    /// place every `models.toml` entry is parsed regardless of which writer
    /// produced it; this pins that an implausible reading never survives it.
    #[test]
    fn load_models_clamps_an_implausible_context_tokens_reading() {
        let mut file = tempfile::NamedTempFile::new().expect("create temp file");
        write!(
            file,
            r#"
[[model]]
id = "hostile-gateway"
provider = "openai-compatible"
base_url = "https://gateway.example/v1"
model = "some-model"
api_key_env = ""
context_tokens = 999999999999999
"#
        )
        .expect("write temp file");

        let configs = load_models(file.path()).expect("parse models.toml");
        assert_eq!(
            configs[0].context_tokens,
            Some(MAX_PLAUSIBLE_CONTEXT_TOKENS),
            "an implausible reading must be clamped, not passed through to num_ctx verbatim"
        );
    }

    #[test]
    fn clamp_context_tokens_passes_none_and_plausible_values_through_unchanged() {
        assert_eq!(clamp_context_tokens(None), None);
        assert_eq!(clamp_context_tokens(Some(200_000)), Some(200_000));
        assert_eq!(
            clamp_context_tokens(Some(MAX_PLAUSIBLE_CONTEXT_TOKENS)),
            Some(MAX_PLAUSIBLE_CONTEXT_TOKENS),
            "the ceiling itself is not clamped down further"
        );
    }

    /// [`ModelRegistry::context_tokens_for`] applies the TIGHTER of the two
    /// ceilings: an implausible-but-under-the-absolute-max reading (below
    /// [`MAX_PLAUSIBLE_CONTEXT_TOKENS`], so [`clamp_context_tokens`] alone
    /// would not catch it) must still be capped to the specific catalog row's
    /// own documented `context_tokens` when the config names a known
    /// `provider_id` + provider-side `model`.
    #[cfg(feature = "provider-openai")]
    #[test]
    fn context_tokens_for_clamps_to_the_specific_catalog_rows_ceiling() {
        let provider_toml = r#"
[[provider]]
id = "anthropic"
name = "Anthropic (Claude)"
protocol = "anthropic"
base_url = "https://api.anthropic.com"
[[provider.auth]]
kind = "api_key"
env = ["ANTHROPIC_API_KEY_UNSET_TEST_3"]
header = "x-api-key"
prefix = ""

[[model]]
id = "claude-opus-5"
provider_id = "anthropic"
context_tokens = 1000000
"#;
        let file: codypendent_providers::ProvidersFile =
            toml::from_str(provider_toml).expect("provider toml");
        let catalog = Catalog::from_parts(file.providers, file.models);

        let id = model_id("anthropic/claude-opus-5");
        let registry = ModelRegistry::new([ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            model: "claude-opus-5".to_string(),
            api_key_env: String::new(),
            // A live /models response that overstated the real 1,000,000
            // ceiling but stayed under the absolute MAX_PLAUSIBLE_CONTEXT_TOKENS
            // sanity clamp — the case only the catalog-aware clamp catches.
            context_tokens: Some(1_900_000),
            provider_id: Some("anthropic".to_string()),
        }])
        .with_catalog(catalog);

        assert_eq!(
            registry.context_tokens_for(&id),
            Some(1_000_000),
            "must clamp to the catalog row's own documented ceiling, tighter than the absolute max"
        );
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
            context_tokens: None,
            provider_id: None,
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
            context_tokens: None,
            provider_id: None,
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
    async fn auth_json_key_resolves_even_when_the_env_var_is_unset() {
        use crate::auth::AuthStore;
        // A unique, never-set var: env alone cannot satisfy this model.
        let var = "CODYPENDENT_TEST_MODELS_AUTHJSON_UNSET_5b2e";
        assert!(std::env::var(var).is_err(), "precondition: {var} unset");

        let id = model_id("groq/llama");
        let cfg = ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".to_string(),
            base_url: "https://api.groq.com/openai/v1".to_string(),
            model: "llama-3.1-8b".to_string(),
            api_key_env: var.to_string(),
            context_tokens: None,
            provider_id: None,
        };

        // Without an auth.json entry the env var is required and missing → error.
        let registry = ModelRegistry::new([cfg.clone()]);
        assert!(
            matches!(
                registry.client_for(&id).await,
                Err(ModelsError::MissingApiKeyEnv { .. })
            ),
            "with no auth.json entry the env path is unchanged (missing → error)"
        );

        // With an auth.json entry the key resolves from it — env never consulted.
        let mut auth = AuthStore::default();
        auth.set("groq/llama", "sk-from-authjson");
        let registry = ModelRegistry::new([cfg]).with_auth(auth);
        assert!(
            registry.client_for(&id).await.is_ok(),
            "an auth.json key must satisfy a model whose api_key_env is unset"
        );
    }

    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn local_model_needs_no_key_with_an_empty_auth_store() {
        use crate::auth::AuthStore;
        let id = model_id("ollama/qwen");
        let registry = ModelRegistry::new([ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            model: "qwen2.5-coder:14b".to_string(),
            api_key_env: String::new(),
            context_tokens: None,
            provider_id: None,
        }])
        .with_auth(AuthStore::default());
        assert!(
            registry.client_for(&id).await.is_ok(),
            "a local model (empty api_key_env, empty auth.json) needs no key"
        );
    }

    /// M1 (defense-in-depth): a hand-edited `auth.json` entry whose value is
    /// the EMPTY string must be treated as ABSENT, never as a present ""
    /// key — otherwise it would silently shadow a perfectly valid
    /// `api_key_env` into "no key". Proven two ways: (1) with the env var
    /// actually unset, an empty auth.json entry must still report
    /// `MissingApiKeyEnv` — exactly as if there were no auth.json entry at
    /// all (this is the discriminating half: without the `.filter(|k|
    /// !k.is_empty())` guard, the empty "" entry would be taken as a real
    /// key and this would wrongly resolve `Ok`); (2) with the SAME empty
    /// entry but the env var now set, resolution still succeeds — the
    /// empty entry never blocks a valid env var either.
    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn empty_auth_json_entry_is_ignored_not_treated_as_a_present_key() {
        use crate::auth::AuthStore;
        // A deliberately unique var name (mirrors
        // `codypendent_providers::credential::api_key_resolves_the_first_set_env_var`,
        // which uses the same set/remove-around-the-assertion pattern).
        let var = "CODYPENDENT_TEST_MODELS_RS_EMPTY_AUTH_FILTER_2f9d";
        assert!(std::env::var(var).is_err(), "precondition: {var} unset");

        let id = model_id("groq/empty-auth-entry");
        let cfg = ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".to_string(),
            base_url: "https://api.groq.com/openai/v1".to_string(),
            model: "llama-3.1-8b".to_string(),
            api_key_env: var.to_string(),
            context_tokens: None,
            provider_id: None,
        };

        let mut auth = AuthStore::default();
        auth.set(id.0.as_str(), ""); // present, but empty
        let registry = ModelRegistry::new([cfg]).with_auth(auth);

        // (1) Env still unset: the empty entry must NOT count as a key, so this
        // must fail exactly like "no auth.json entry at all" — never silently
        // succeed with an empty key.
        assert!(
            matches!(
                registry.client_for(&id).await,
                Err(ModelsError::MissingApiKeyEnv { .. })
            ),
            "an empty auth.json entry must be ignored, falling through to the (missing) env var"
        );

        // (2) Now set the env var: the same empty entry must not shadow it.
        std::env::set_var(var, "sk-from-env-2f9d");
        assert!(
            registry.client_for(&id).await.is_ok(),
            "a set env var must still resolve when the auth.json entry for this id is empty"
        );
        std::env::remove_var(var);
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
            context_tokens: None,
            provider_id: None,
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
                context_tokens: None,
                provider_id: None,
            },
            ModelConfig {
                id: reachable.clone(),
                provider: "openai-compatible".to_string(),
                base_url: format!("http://{reachable_addr}/v1"),
                model: "unused".to_string(),
                api_key_env: String::new(),
                context_tokens: None,
                provider_id: None,
            },
        ]);
        let policy = ModelPolicy::new()
            .with_candidates(AgentMode::Build, vec![closed.clone(), reachable.clone()]);

        let resolved = resolve_model_with_probe(
            &registry,
            &policy,
            AgentMode::Build,
            &TcpConnectProbe::default(),
        )
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
            context_tokens: None,
            provider_id: None,
        }]);
        let policy = ModelPolicy::new().with_candidates(AgentMode::Build, vec![closed.clone()]);

        let err = resolve_model(&registry, &policy, AgentMode::Build)
            .await
            .expect_err("no reachable candidate");
        match err {
            ModelsError::AllCandidatesFailed { mode, attempts } => {
                assert_eq!(mode, AgentMode::Build);
                assert_eq!(attempts.len(), 1);
                assert_eq!(attempts[0].id, closed);
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
            context_tokens: None,
            provider_id: None,
        }]);
        let ghost = model_id("not-in-registry");
        let policy =
            ModelPolicy::new().with_candidates(AgentMode::Plan, vec![ghost, reachable.clone()]);

        let resolved = resolve_model_with_probe(
            &registry,
            &policy,
            AgentMode::Plan,
            &TcpConnectProbe::default(),
        )
        .await
        .expect("second, registered candidate should resolve");
        assert_eq!(resolved.id, reachable);

        drop(listener);
    }

    #[cfg(feature = "provider-openai")]
    async fn models_server(models: &[&str]) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let data = models
            .iter()
            .map(|id| serde_json::json!({ "id": id }))
            .collect::<Vec<_>>();
        let body = serde_json::json!({ "data": data }).to_string();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        (format!("http://{address}/v1"), task)
    }

    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn resolve_model_falls_back_past_a_reachable_but_missing_model() {
        let stale = model_id("stale");
        let healthy = model_id("healthy");
        let (stale_url, stale_server) = models_server(&["something-else"]).await;
        let (healthy_url, healthy_server) = models_server(&["installed-model"]).await;
        let registry = ModelRegistry::new([
            ModelConfig {
                id: stale.clone(),
                provider: "openai-compatible".to_string(),
                base_url: stale_url,
                model: "missing-model".to_string(),
                api_key_env: String::new(),
                context_tokens: None,
                provider_id: None,
            },
            ModelConfig {
                id: healthy.clone(),
                provider: "openai-compatible".to_string(),
                base_url: healthy_url,
                model: "installed-model".to_string(),
                api_key_env: String::new(),
                context_tokens: None,
                provider_id: None,
            },
        ]);
        let policy =
            ModelPolicy::new().with_candidates(AgentMode::Build, vec![stale, healthy.clone()]);

        let resolved = resolve_model(&registry, &policy, AgentMode::Build)
            .await
            .expect("the installed fallback should be selected");
        assert_eq!(resolved.id, healthy);
        stale_server.await.unwrap();
        healthy_server.await.unwrap();
    }

    // -- models.toml extras: [embedding] + [retrieval] ---------------------

    #[test]
    fn extras_parse_embedding_and_retrieval_alongside_model_entries() {
        let mut file = tempfile::NamedTempFile::new().expect("create temp file");
        write!(
            file,
            r#"
[[model]]
id = "local-default"
provider = "openai-compatible"
base_url = "http://localhost:11434/v1"
model = "qwen2.5-coder:14b"
api_key_env = ""

[embedding]
base_url = "http://localhost:11434/v1"
model = "nomic-embed-text"
dims = 768

[retrieval]
mcp_top_k = 5
"#
        )
        .expect("write temp file");

        // Both readers coexist on one file: `load_models` ignores the new
        // tables; `load_model_extras` ignores the [[model]] entries.
        let models = load_models(file.path()).expect("model entries still parse");
        assert_eq!(models.len(), 1);
        let extras = load_model_extras(file.path()).expect("extras parse");
        let embedding = extras.embedding.expect("embedding entry present");
        assert_eq!(embedding.provider, "openai-compatible", "provider defaults");
        assert_eq!(embedding.base_url, "http://localhost:11434/v1");
        assert_eq!(embedding.model, "nomic-embed-text");
        assert_eq!(embedding.api_key_env, "", "api_key_env defaults empty");
        assert_eq!(embedding.dims, Some(768));
        assert_eq!(extras.retrieval.mcp_top_k, 5);
    }

    #[test]
    fn extras_default_when_tables_or_file_are_absent() {
        // A models.toml with only [[model]] entries (the existing shape) yields
        // the defaults: no embedding model, default MCP top-k.
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
"#
        )
        .expect("write temp file");
        let extras = load_model_extras(file.path()).expect("extras parse");
        assert_eq!(extras.embedding, None);
        assert_eq!(extras.retrieval.mcp_top_k, DEFAULT_MCP_TOP_K);

        // No file at all: defaults too (context assembly must not need one).
        let dir = tempfile::tempdir().expect("tempdir");
        let extras =
            load_model_extras(&dir.path().join("models.toml")).expect("absent file defaults");
        assert_eq!(extras, ModelExtras::default());
    }

    #[test]
    fn malformed_extras_are_a_legible_error_not_a_silent_default() {
        let mut file = tempfile::NamedTempFile::new().expect("create temp file");
        write!(file, "[embedding]\nmodel = 42\n").expect("write temp file");
        assert!(
            matches!(
                load_model_extras(file.path()),
                Err(ModelsError::ParseConfig { .. })
            ),
            "a typo'd [embedding] table must error, never silently disable embeddings"
        );
    }

    // -- failure taxonomy (transient vs permanent) -------------------------

    #[test]
    fn classify_provider_message_marks_connect_timeout_and_5xx_transient() {
        for message in [
            "connection refused (os error 111)",
            "Connection reset by peer",
            "operation timed out",
            "request timeout",
            "provider returned HTTP 429 Too Many Requests from /models",
            "HTTP status server error (503 Service Unavailable)",
            "upstream 502",
            "the model is overloaded, please retry",
        ] {
            assert_eq!(
                classify_provider_message(message),
                FailureClass::Transient,
                "expected transient: {message}"
            );
        }
    }

    #[test]
    fn classify_provider_message_marks_contract_failures_permanent() {
        for message in [
            "provider returned HTTP 401 Unauthorized from /models",
            "invalid request: unknown model `nope`",
            "provider returned HTTP 404 from /models",
            "content policy violation",
            // A transient-looking digit run embedded in a longer number must
            // NOT match — status codes are standalone digit runs only.
            "request id 15005 was rejected: bad schema",
        ] {
            assert_eq!(
                classify_provider_message(message),
                FailureClass::Permanent,
                "expected permanent: {message}"
            );
        }
    }

    #[test]
    fn models_error_failure_class_follows_the_variant_taxonomy() {
        let transient = ModelsError::ConnectionFailed {
            base_url: "http://localhost:11434/v1".to_string(),
            reason: "connection refused".to_string(),
        };
        assert_eq!(transient.failure_class(), FailureClass::Transient);

        // ModelUnavailable defers to its provider-supplied reason: a 5xx from
        // /models is a blip worth retrying, a missing model tag is not.
        let blip = ModelsError::ModelUnavailable {
            model: model_id("m"),
            provider_model: "m".to_string(),
            reason: "provider returned HTTP 503 from /models".to_string(),
        };
        assert_eq!(blip.failure_class(), FailureClass::Transient);
        let missing = ModelsError::ModelUnavailable {
            model: model_id("m"),
            provider_model: "m".to_string(),
            reason: "provider did not list this model".to_string(),
        };
        assert_eq!(missing.failure_class(), FailureClass::Permanent);

        // Config/credential failures can never be fixed by waiting.
        let permanent = ModelsError::MissingApiKeyEnv {
            model: model_id("m"),
            var: "SOME_KEY".to_string(),
        };
        assert_eq!(permanent.failure_class(), FailureClass::Permanent);
        assert_eq!(
            ModelsError::UnknownModel(model_id("nope")).failure_class(),
            FailureClass::Permanent
        );
    }

    // -- catalog-resolved auth headers (provider_id) ------------------------

    /// A one-request HTTP server that answers `body` and hands back the raw
    /// request head it received, so a test can assert on the exact auth
    /// headers a probe/client sent.
    #[cfg(feature = "provider-openai")]
    async fn capture_server(body: &str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = body.to_string();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 4096];
                let n = stream.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..n]);
                let head_end = request
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|p| p + 4);
                let content_len = String::from_utf8_lossy(&request).lines().find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                });
                if head_end.is_some_and(|head| request.len() >= head + content_len.unwrap_or(0)) {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (format!("http://{address}/v1"), task)
    }

    /// [`native_capture_server`] with extra response headers, for the paths
    /// that read a header rather than the body.
    #[cfg(feature = "provider-openai")]
    async fn native_capture_server_with_headers(
        status: &str,
        content_type: &str,
        body: String,
        extra_headers: &[(&str, &str)],
    ) -> (String, tokio::task::JoinHandle<String>) {
        let extra: String = extra_headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect();
        native_capture_server_inner(status, content_type, body, extra).await
    }

    #[cfg(feature = "provider-openai")]
    async fn native_capture_server(
        status: &str,
        content_type: &str,
        body: String,
    ) -> (String, tokio::task::JoinHandle<String>) {
        native_capture_server_inner(status, content_type, body, String::new()).await
    }

    #[cfg(feature = "provider-openai")]
    async fn native_capture_server_inner(
        status: &str,
        content_type: &str,
        body: String,
        extra_headers: String,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        let content_type = content_type.to_string();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 4096];
                let n = stream.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..n]);
                let head_end = request
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|p| p + 4);
                let content_len = String::from_utf8_lossy(&request).lines().find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                });
                if head_end.is_some_and(|head| request.len() >= head + content_len.unwrap_or(0)) {
                    break;
                }
            }
            let response = format!("HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n{extra_headers}connection: close\r\n\r\n{body}", body.len());
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (format!("http://{address}/v1beta"), task)
    }

    #[cfg(feature = "provider-openai")]
    async fn bytewise_native_server(body: String) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.set_nodelay(true).unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 4096];
                let n = stream.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..n]);
                let head_end = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|position| position + 4);
                let content_len = String::from_utf8_lossy(&request).lines().find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                });
                if head_end.is_some_and(|head| request.len() >= head + content_len.unwrap_or(0)) {
                    break;
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            for byte in body.bytes() {
                if stream.write_all(&[byte]).await.is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            String::from_utf8_lossy(&request).into_owned()
        });
        (format!("http://{address}/v1beta"), task)
    }

    /// The HIGH-severity auth-flatten regression: an azure-shaped provider
    /// (catalog auth `header = "api-key"`, `prefix = ""` — the built-in
    /// `azure-openai` entry) must round-trip its key under `api-key`, not
    /// under a hardcoded `Authorization: Bearer`. `check_model` is asserted
    /// on the wire; `client_for` must agree because both resolve the same
    /// [`EndpointAuth`].
    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn check_model_sends_the_catalog_declared_api_key_header_for_azure() {
        use crate::auth::AuthStore;
        let id = model_id("azure-openai/gpt-5.1");
        let (url, server) = capture_server(r#"{"data":[{"id":"gpt-5.1"}]}"#).await;
        let mut auth = AuthStore::default();
        auth.set(id.0.as_str(), "azure-secret-key");
        let registry = ModelRegistry::new([ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".to_string(),
            base_url: url,
            model: "gpt-5.1".to_string(),
            api_key_env: String::new(),
            context_tokens: None,
            provider_id: Some("azure-openai".to_string()),
        }])
        .with_auth(auth);

        registry
            .check_model(&id)
            .await
            .expect("the endpoint lists the model");
        let request = server.await.unwrap();
        let head = request.to_lowercase();
        assert!(
            head.contains("api-key: azure-secret-key"),
            "the key must ride the catalog-declared `api-key` header:\n{request}"
        );
        assert!(
            !head.contains("authorization:"),
            "no bearer header may be sent for an api-key provider:\n{request}"
        );
    }

    /// The legacy shape (no `provider_id`) keeps the exact bearer wire
    /// behavior — the back-compat half of the azure round-trip test.
    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn check_model_defaults_to_bearer_when_no_provider_id_is_set() {
        use crate::auth::AuthStore;
        let id = model_id("groq/llama");
        let (url, server) = capture_server(r#"{"data":[{"id":"llama-3.1-8b"}]}"#).await;
        let mut auth = AuthStore::default();
        auth.set(id.0.as_str(), "sk-legacy");
        let registry = ModelRegistry::new([ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".to_string(),
            base_url: url,
            model: "llama-3.1-8b".to_string(),
            api_key_env: String::new(),
            context_tokens: None,
            provider_id: None,
        }])
        .with_auth(auth);

        registry.check_model(&id).await.expect("listed");
        let request = server.await.unwrap();
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer sk-legacy"),
            "a legacy entry keeps `Authorization: Bearer`:\n{request}"
        );
    }

    /// `client_for` completes a chat call through the header-aware client for
    /// an azure-shaped provider: the mock asserts `api-key` (no bearer) and
    /// the provider's extra headers on the actual `/chat/completions` POST.
    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn client_for_sends_catalog_headers_on_chat_completions() {
        use crate::auth::AuthStore;
        use agent_framework_core::client::ChatClient;
        use agent_framework_core::types::{ChatOptions, Message};

        // A user-shadowed azure-shaped provider carrying an extra header, so
        // both the auth header and extra_headers are exercised in one call.
        let provider_toml = r#"
[[provider]]
id = "azure-shaped"
name = "Azure-shaped"
protocol = "openai-chat"
base_url = "https://unused.example/v1"
extra_headers = { "x-extra-version" = "2026-01-01" }
[[provider.auth]]
kind = "api_key"
env = ["AZURE_SHAPED_KEY_UNSET_TEST"]
header = "api-key"
prefix = ""
"#;
        let file: codypendent_providers::ProvidersFile =
            toml::from_str(provider_toml).expect("provider toml");
        let catalog = Catalog::from_providers(file.providers);

        let body = r#"{"id":"resp-1","model":"gpt-5.1","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#;
        let (url, server) = capture_server(body).await;

        let id = model_id("azure-shaped/gpt-5.1");
        let mut auth = AuthStore::default();
        auth.set(id.0.as_str(), "azure-secret-key");
        let registry = ModelRegistry::new([ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".to_string(),
            base_url: url,
            model: "gpt-5.1".to_string(),
            api_key_env: String::new(),
            context_tokens: None,
            provider_id: Some("azure-shaped".to_string()),
        }])
        .with_auth(auth)
        .with_catalog(catalog);

        let client = registry.client_for(&id).await.expect("client builds");
        let response = client
            .get_response(vec![Message::user("ping")], ChatOptions::default())
            .await
            .expect("the mock answers");
        assert_eq!(response.text(), "hi");

        let request = server.await.unwrap();
        let head = request.to_lowercase();
        assert!(
            head.starts_with("post /v1/chat/completions"),
            "the chat route is the base_url sibling:\n{request}"
        );
        assert!(
            head.contains("api-key: azure-secret-key"),
            "the key rides the catalog-declared header:\n{request}"
        );
        assert!(
            head.contains("x-extra-version: 2026-01-01"),
            "provider extra headers are sent:\n{request}"
        );
        assert!(
            !head.contains("authorization:"),
            "no bearer header for an api-key provider:\n{request}"
        );
    }

    /// F3 (`2026-08-13-verticals/acp-models.md`): `codypendent models add
    /// anthropic claude-opus-5` writes `provider = "openai-compatible"` (every
    /// `models add` entry does — see `crates/cli/src/commands.rs::models_add`)
    /// with `provider_id = Some("anthropic")`. Before this fix,
    /// `config_to_protocol_auth` ignored `provider_id` entirely and hard-coded
    /// `Protocol::OpenAiChat`, so this exact config would have POSTed an
    /// OpenAI-shaped chat-completions body to `/chat/completions` with a
    /// bearer header — wrong on every axis for Anthropic's real Messages API.
    /// This test builds a client from that exact on-disk shape and asserts on
    /// the real wire: the real Messages route, `x-api-key` (never bearer), and
    /// the catalog's `anthropic-version` extra header.
    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn client_for_speaks_the_anthropic_wire_for_a_models_add_style_config() {
        use crate::auth::AuthStore;
        use agent_framework_core::client::ChatClient;
        use agent_framework_core::types::{ChatOptions, Message};

        let provider_toml = r#"
[[provider]]
id = "anthropic"
name = "Anthropic (Claude)"
protocol = "anthropic"
base_url = "https://unused.example"
extra_headers = { "anthropic-version" = "2099-12-31", "x-catalog-extra" = "native" }
query_params = { "audience" = "team alpha", "reserved" = "a&b" }
[[provider.auth]]
kind = "api_key"
env = ["ANTHROPIC_API_KEY_UNSET_TEST"]
header = "x-catalog-key"
prefix = "Token "
"#;
        let file: codypendent_providers::ProvidersFile =
            toml::from_str(provider_toml).expect("provider toml");
        let catalog = Catalog::from_providers(file.providers);

        let body = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5",
            "content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn",
            "usage":{"input_tokens":1,"output_tokens":1}}"#;
        let (url, server) = capture_server(body).await;
        // `capture_server` returns a `/v1`-suffixed base_url for the OpenAI-chat
        // tests above; Anthropic's own base_url is bare (`https://api.anthropic.com`,
        // no `/v1` — the client appends `/v1/messages` itself), so strip it here.
        let base_url = url.trim_end_matches("/v1").to_string();

        // Exactly the shape `models_add` (`crates/cli/src/commands.rs`) writes:
        // `provider = "openai-compatible"`, `provider_id = Some("anthropic")`.
        let id = model_id("anthropic/claude-opus-5");
        let mut auth = AuthStore::default();
        auth.set(id.0.as_str(), "sk-ant-secret");
        let registry = ModelRegistry::new([ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".to_string(),
            base_url,
            model: "claude-opus-5".to_string(),
            api_key_env: String::new(),
            context_tokens: None,
            provider_id: Some("anthropic".to_string()),
        }])
        .with_auth(auth)
        .with_catalog(catalog);

        let client = registry.client_for(&id).await.expect("client builds");
        let response = client
            .get_response(vec![Message::user("ping")], ChatOptions::default())
            .await
            .expect("the mock answers");
        assert_eq!(response.text(), "hi");

        let request = server.await.unwrap();
        let head = request.to_lowercase();
        assert!(
            head.starts_with("post /v1/messages"),
            "the Anthropic Messages route, not /chat/completions:\n{request}"
        );
        assert!(
            head.contains("x-catalog-key: token sk-ant-secret"),
            "the key uses the catalog header and prefix:\n{request}"
        );
        assert!(
            head.contains("anthropic-version: 2099-12-31"),
            "the catalog's anthropic-version override wins:\n{request}"
        );
        assert!(head.contains("x-catalog-extra: native"));
        assert!(head.starts_with("post /v1/messages?audience=team+alpha&reserved=a%26b "));
        assert!(!head.contains("x-api-key:"));
        assert!(!head.contains("anthropic-version: 2023-06-01"));
        assert!(
            !head.contains("authorization:"),
            "no bearer header may be sent to an Anthropic endpoint:\n{request}"
        );
        let payload: serde_json::Value =
            serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            payload
                .pointer("/messages/0/content/0/text")
                .and_then(|v| v.as_str()),
            Some("ping")
        );
    }

    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn anthropic_sse_client_uses_catalog_metadata_and_normalizes_fragmented_stream() {
        use crate::auth::AuthStore;
        use agent_framework_core::types::{ChatOptions, ChatResponse, Message};
        use futures::StreamExt;

        let providers: codypendent_providers::ProvidersFile = toml::from_str(
            r#"
[[provider]]
id = "anthropic-stream"
name = "Anthropic stream"
protocol = "anthropic"
base_url = "https://unused.example"
extra_headers = { "anthropic-version" = "2099-12-31", "x-stream-extra" = "yes" }
query_params = { "audience" = "stream test" }
[[provider.auth]]
kind = "api_key"
env = ["ANTHROPIC_STREAM_UNSET_TEST"]
header = "x-stream-key"
prefix = "Token "
"#,
        )
        .unwrap();
        let stream_body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-1\",\"name\":\"lookup\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\":\\\"rust\\\"}\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        )
        .to_owned();
        let (base_url, server) = bytewise_native_server(stream_body).await;
        let id = model_id("anthropic-stream/claude");
        let mut auth = AuthStore::default();
        auth.set(id.0.as_str(), "stream-secret");
        let registry = ModelRegistry::new([ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".into(),
            base_url: base_url.trim_end_matches("/v1beta").to_owned(),
            model: "claude-test".into(),
            api_key_env: String::new(),
            provider_id: Some("anthropic-stream".into()),
            context_tokens: None,
        }])
        .with_auth(auth)
        .with_catalog(Catalog::from_providers(providers.providers));

        let updates = registry
            .client_for(&id)
            .await
            .unwrap()
            .get_streaming_response(vec![Message::user("ping")], ChatOptions::default())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<agent_framework_core::error::Result<Vec<_>>>()
            .unwrap();
        let response = ChatResponse::from_updates(updates);
        assert_eq!(response.text(), "hello");
        let call = &response.function_calls()[0];
        assert_eq!(call.call_id, "call-1");
        assert_eq!(call.name, "lookup");
        assert_eq!(
            call.parse_arguments().unwrap(),
            HashMap::from([("q".to_owned(), serde_json::json!("rust"))])
        );

        let request = server.await.unwrap();
        let lower = request.to_lowercase();
        assert!(lower.starts_with("post /v1/messages?audience=stream+test "));
        assert!(lower.contains("x-stream-key: token stream-secret"));
        assert!(lower.contains("x-stream-extra: yes"));
        assert!(lower.contains("anthropic-version: 2099-12-31"));
        let payload: serde_json::Value =
            serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(payload["stream"], true);
    }

    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn gemini_native_generate_content_and_stream_are_normalized() {
        use crate::auth::AuthStore;
        use agent_framework_core::types::{ChatOptions, Message};
        use futures::StreamExt;

        let catalog: codypendent_providers::ProvidersFile = toml::from_str(
            r#"
[[provider]]
id = "gemini"
name = "Gemini"
protocol = "gemini-native"
base_url = "https://unused.example/v1beta"
extra_headers = { "x-gemini-extra" = "catalog" }
query_params = { "region" = "north west", "token" = "a&b" }
[[provider.auth]]
kind = "api_key"
env = ["GEMINI_UNSET_NATIVE_TEST"]
header = "x-gemini-key"
prefix = "Key "
"#,
        )
        .unwrap();
        let id = model_id("gemini/model");
        let mut auth = AuthStore::default();
        auth.set(id.0.as_str(), "gem-secret");
        let response_body =
            r#"{"candidates":[{"content":{"parts":[{"text":"hello"}]}}]}"#.to_string();
        let (base_url, server) =
            native_capture_server("200 OK", "application/json", response_body).await;
        let cfg = ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".into(),
            base_url,
            model: "gemini-test".into(),
            api_key_env: String::new(),
            provider_id: Some("gemini".into()),
            context_tokens: None,
        };
        let registry = ModelRegistry::new([cfg.clone()])
            .with_auth(auth.clone())
            .with_catalog(Catalog::from_providers(catalog.providers.clone()));
        let client = registry.client_for(&id).await.unwrap();
        assert_eq!(
            client
                .get_response(vec![Message::user("ping")], ChatOptions::default())
                .await
                .unwrap()
                .text(),
            "hello"
        );
        let request = server.await.unwrap();
        let lower = request.to_lowercase();
        assert!(lower.starts_with(
            "post /v1beta/models/gemini-test:generatecontent?region=north+west&token=a%26b "
        ));
        assert!(lower.contains("x-gemini-key: key gem-secret"));
        assert!(lower.contains("x-gemini-extra: catalog"));
        assert!(!lower.contains("x-goog-api-key:"));
        assert!(!lower.contains("authorization:"));
        let payload: serde_json::Value =
            serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            payload
                .pointer("/contents/0/parts/0/text")
                .and_then(|v| v.as_str()),
            Some("ping")
        );

        let stream_body = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"a\"}]}}]}\n\ndata: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"b\"}]},\"finishReason\":\"STOP\"}]}\n\n".to_string();
        let (base_url, stream_server) =
            native_capture_server("200 OK", "text/event-stream", stream_body).await;
        let registry = ModelRegistry::new([ModelConfig { base_url, ..cfg }])
            .with_auth(auth)
            .with_catalog(Catalog::from_providers(catalog.providers));
        let chunks = registry
            .client_for(&id)
            .await
            .unwrap()
            .get_streaming_response(vec![Message::user("ping")], ChatOptions::default())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert_eq!(
            chunks
                .into_iter()
                .map(|u| u.unwrap().text_content())
                .collect::<String>(),
            "ab"
        );
        let stream_request = stream_server.await.unwrap();
        assert!(stream_request.starts_with(
            "POST /v1beta/models/gemini-test:streamGenerateContent?alt=sse&region=north+west&token=a%26b "
        ));
    }

    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn native_provider_error_body_is_bounded() {
        use agent_framework_core::client::ChatClient;
        use agent_framework_core::types::{ChatOptions, Message};
        let cfg = ModelConfig {
            id: model_id("gemini/error"),
            provider: "openai-compatible".into(),
            base_url: String::new(),
            model: "gemini-test".into(),
            api_key_env: String::new(),
            provider_id: Some("gemini".into()),
            context_tokens: None,
        };
        let (base_url, server) =
            native_capture_server("500 Internal Server Error", "text/plain", "X".repeat(5000))
                .await;
        let client = NativeChatClient::new(
            &ModelConfig { base_url, ..cfg },
            NativeProtocol::Gemini,
            "secret",
        )
        .unwrap();
        let error = client
            .get_response(vec![Message::user("ping")], ChatOptions::default())
            .await
            .unwrap_err()
            .to_string();
        server.await.unwrap();
        assert!(
            error.len() < 1200,
            "bounded public error: {} bytes",
            error.len()
        );
        assert!(!error.contains("secret"));
    }

    /// A native provider that answers 429 with `Retry-After` must have that
    /// wait honored.
    ///
    /// The OpenAI-compatible client reads the header and embeds the hint the
    /// retry module parses back out; the native client ignored it entirely, so
    /// a rate-limited native provider was retried on the generic backoff
    /// schedule — hammering a server that had just said "not yet" and named a
    /// time.
    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn a_native_provider_rate_limit_carries_its_retry_after() {
        use agent_framework_core::client::ChatClient;
        use agent_framework_core::types::{ChatOptions, Message};

        let (base_url, server) = native_capture_server_with_headers(
            "429 Too Many Requests",
            "text/plain",
            "slow down".to_string(),
            &[("retry-after", "2")],
        )
        .await;
        let client = NativeChatClient::new(
            &ModelConfig {
                id: model_id("gemini/rate-limited"),
                provider: "openai-compatible".into(),
                base_url,
                model: "gemini-test".into(),
                api_key_env: String::new(),
                provider_id: Some("gemini".into()),
                context_tokens: None,
            },
            NativeProtocol::Gemini,
            "secret",
        )
        .unwrap();

        let error = client
            .get_response(vec![Message::user("ping")], ChatOptions::default())
            .await
            .unwrap_err()
            .to_string();
        server.await.unwrap();

        assert!(
            error.contains("[retry-after-ms=2000]"),
            "the server's stated wait must reach the retry module: {error}"
        );
        assert!(
            error.contains(codypendent_providers::retry::RETRY_AFTER_MARKER),
            "the marker must be the one the retry parser looks for: {error}"
        );
        assert!(!error.contains("secret"), "the credential never leaks");
    }

    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn native_provider_error_redacts_raw_key_with_custom_prefix() {
        use agent_framework_core::client::ChatClient;
        use agent_framework_core::types::{ChatOptions, Message};

        let raw_key = "raw-custom-prefix-secret";
        let (base_url, server) = native_capture_server(
            "401 Unauthorized",
            "text/plain",
            format!("credential {raw_key} was rejected"),
        )
        .await;
        let cfg = ModelConfig {
            id: model_id("anthropic/redaction"),
            provider: "openai-compatible".into(),
            base_url: base_url.trim_end_matches("/v1beta").to_owned(),
            model: "test".into(),
            api_key_env: String::new(),
            provider_id: Some("anthropic".into()),
            context_tokens: None,
        };
        let client = NativeChatClient::new_with_credential(
            &cfg,
            NativeProtocol::Anthropic,
            ResolvedCredential::ApiKey {
                header: "x-custom-key".into(),
                prefix: "Custom ".into(),
                value: raw_key.into(),
            },
            &EndpointAuth {
                header: "x-custom-key".into(),
                prefix: "Custom ".into(),
                extra_headers: BTreeMap::new(),
                query_params: BTreeMap::new(),
                provider_env: Vec::new(),
                requires_api_key: true,
            },
        )
        .unwrap();
        assert!(!format!("{client:?}").contains(raw_key));
        let error = client
            .get_response(vec![Message::user("ping")], ChatOptions::default())
            .await
            .unwrap_err()
            .to_string();
        server.await.unwrap();
        assert!(!error.contains(raw_key));
        assert!(error.contains("<redacted>"));
    }

    #[cfg(feature = "provider-openai")]
    #[test]
    fn sse_decoder_bounds_lines_and_multiline_events() {
        let wire = b": comment\r\ndata: {\"a\":\r\ndata: 1}\r\n\r\n";
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        for byte in wire {
            events.extend(decoder.push(&[*byte], false).unwrap());
        }
        events.extend(decoder.push(&[], true).unwrap());
        assert_eq!(events, vec!["{\"a\":\n1}"]);

        let mut decoder = SseDecoder::default();
        assert!(decoder
            .push(&vec![b'x'; MAX_SSE_PENDING_LINE_BYTES + 1], false)
            .unwrap_err()
            .to_string()
            .contains("line exceeded"));
        let mut decoder = SseDecoder::default();
        let line = format!("data: {}\n", "x".repeat(MAX_SSE_EVENT_DATA_BYTES / 2 + 1));
        decoder.push(line.as_bytes(), false).unwrap();
        assert!(decoder.push(line.as_bytes(), false).is_err());
    }

    #[cfg(feature = "provider-openai")]
    #[test]
    fn anthropic_interleaved_tool_deltas_merge_by_block_index() {
        use agent_framework_core::types::ChatResponse;
        let events = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call-a","name":"alpha","input":{}}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call-b","name":"beta","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"b\":2}"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"1}"}}"#,
        ];
        let mut normalizer = StreamNormalizer::new(NativeProtocol::Anthropic);
        let updates = events
            .into_iter()
            .map(|event| normalizer.normalize(event.into()).unwrap().unwrap())
            .collect();
        let response = ChatResponse::from_updates(updates);
        let calls = response.function_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].call_id, "call-a");
        assert_eq!(calls[0].name, "alpha");
        assert_eq!(
            serde_json::to_value(calls[0].parse_arguments().unwrap()).unwrap(),
            serde_json::json!({"a": 1})
        );
        assert_eq!(calls[1].call_id, "call-b");
        assert_eq!(calls[1].name, "beta");
        assert_eq!(
            serde_json::to_value(calls[1].parse_arguments().unwrap()).unwrap(),
            serde_json::json!({"b": 2})
        );
        assert!(normalizer
            .normalize(r#"{"type":"content_block_delta","index":9,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#.into())
            .is_err());
    }

    #[cfg(feature = "provider-openai")]
    #[test]
    fn anthropic_stream_preserves_initial_tool_input_and_rejects_later_delta() {
        use agent_framework_core::types::ChatResponse;

        let start = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call-a","name":"alpha","input":{"initial":1}}}"#;
        let mut normalizer = StreamNormalizer::new(NativeProtocol::Anthropic);
        let update = normalizer.normalize(start.into()).unwrap().unwrap();
        let response = ChatResponse::from_updates(vec![update]);
        assert_eq!(
            response.function_calls()[0].parse_arguments().unwrap(),
            HashMap::from([("initial".to_owned(), serde_json::json!(1))])
        );

        let delta = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"later\":2}"}}"#;
        assert!(normalizer
            .normalize(delta.into())
            .unwrap_err()
            .to_string()
            .contains("non-empty initial input"));
    }

    /// Native-provider telemetry: `FrameworkModelDriver` derives a run's token
    /// totals and its ROUTED COST exclusively from the assembled response's
    /// usage, so an Anthropic stream whose `message_start`/`message_delta`
    /// usage is dropped reports null tokens and null cost for every run.
    /// Both events carry RUNNING TOTALS, so the assembled figure must be the
    /// provider's final count — not the sum of the two announcements.
    #[cfg(feature = "provider-openai")]
    #[test]
    fn anthropic_stream_usage_reaches_the_assembled_response_once() {
        use agent_framework_core::types::ChatResponse;

        let events = [
            r#"{"type":"message_start","message":{"usage":{"input_tokens":25,"output_tokens":1,"cache_read_input_tokens":7,"cache_creation_input_tokens":3}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":25,"output_tokens":15}}"#,
        ];
        let mut normalizer = StreamNormalizer::new(NativeProtocol::Anthropic);
        let updates = events
            .into_iter()
            .map(|event| normalizer.normalize(event.into()).unwrap().unwrap())
            .collect();
        let response = ChatResponse::from_updates(updates);
        let usage = response
            .usage_details
            .clone()
            .expect("provider usage reaches the assembled response");
        assert_eq!(usage.input_token_count, Some(25));
        assert_eq!(usage.output_token_count, Some(15));
        assert_eq!(usage.total_token_count, Some(40));
        assert_eq!(usage.cache_read_input_token_count, Some(7));
        assert_eq!(usage.cache_creation_input_token_count, Some(3));
        assert_eq!(response.text(), "hi");
    }

    /// `stop_reason: max_tokens` is a CUT-OFF reply, not a finished one. Both
    /// native streams must report it as the framework's `length` finish reason,
    /// or the loop cannot tell a truncated tool call from a complete one.
    #[cfg(feature = "provider-openai")]
    #[test]
    fn a_max_tokens_stop_reaches_the_assembled_response_as_length() {
        use agent_framework_core::types::{ChatResponse, FinishReason};

        let mut anthropic = StreamNormalizer::new(NativeProtocol::Anthropic);
        let updates: Vec<_> = [
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":4096}}"#,
        ]
        .into_iter()
        .map(|event| anthropic.normalize(event.into()).unwrap().unwrap())
        .collect();
        let response = ChatResponse::from_updates(updates);
        assert_eq!(
            response.finish_reason,
            Some(FinishReason::new(FinishReason::LENGTH))
        );

        let mut gemini = StreamNormalizer::new(NativeProtocol::Gemini);
        let update = gemini
            .normalize(
                r#"{"candidates":[{"content":{"parts":[{"text":"partial"}]},"finishReason":"MAX_TOKENS"}]}"#
                    .into(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            ChatResponse::from_updates(vec![update]).finish_reason,
            Some(FinishReason::new(FinishReason::LENGTH))
        );

        // A normal stop is not mistaken for truncation.
        let mut finished = StreamNormalizer::new(NativeProtocol::Anthropic);
        let done = finished
            .normalize(
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#
                    .into(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(ChatResponse::from_updates(vec![done]).finish_reason, None);
    }

    /// A connect failure on the native path must be spelled so the retry
    /// classifier recognises it — and must not echo the URL (Gemini's key is in
    /// the query string).
    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn a_native_connect_failure_is_retryable_and_never_echoes_the_url() {
        use agent_framework_core::client::ChatClient;
        use agent_framework_core::types::{ChatOptions, Message};

        // A port that was just free and is now closed: connection refused.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let client = NativeChatClient::new(
            &ModelConfig {
                id: model_id("anthropic/down"),
                provider: "openai-compatible".into(),
                base_url: format!("http://127.0.0.1:{port}"),
                model: "claude-test".into(),
                api_key_env: String::new(),
                provider_id: Some("anthropic".into()),
                context_tokens: None,
            },
            NativeProtocol::Anthropic,
            "sk-secret-key",
        )
        .unwrap();
        let error = client
            .get_response(vec![Message::user("hi")], ChatOptions::new())
            .await
            .expect_err("nothing is listening");
        let text = error.to_string();
        assert!(text.contains("connection error"), "got {text:?}");
        assert!(
            !text.contains("127.0.0.1"),
            "the URL must not be echoed: {text:?}"
        );
        assert!(!text.contains("sk-secret-key"));
        assert!(
            codypendent_providers::retry::retryable(&text).is_some(),
            "the retry classifier must treat a connect failure as transient: {text:?}"
        );
    }

    /// The Gemini half of the same defect, on both wire shapes: a
    /// `generateContent` body's `usageMetadata` becomes the response's usage,
    /// and a stream that repeats its running totals on every chunk is counted
    /// ONCE. Gemini reports thinking tokens outside `candidatesTokenCount`, so
    /// the output count is their sum (reasoning also reported on its own field).
    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn gemini_usage_metadata_is_mapped_on_both_wire_shapes() {
        use agent_framework_core::client::ChatClient;
        use agent_framework_core::types::{ChatOptions, ChatResponse, Message};

        let body = serde_json::json!({
            "candidates": [{"content": {"parts": [{"text": "hello"}]}}],
            "usageMetadata": {
                "promptTokenCount": 11,
                "candidatesTokenCount": 4,
                "thoughtsTokenCount": 2,
                "cachedContentTokenCount": 5,
                "totalTokenCount": 17
            }
        })
        .to_string();
        let (base_url, server) = native_capture_server("200 OK", "application/json", body).await;
        let client = NativeChatClient::new(
            &ModelConfig {
                id: model_id("gemini/usage"),
                provider: "openai-compatible".into(),
                base_url,
                model: "gemini-test".into(),
                api_key_env: String::new(),
                provider_id: Some("gemini".into()),
                context_tokens: None,
            },
            NativeProtocol::Gemini,
            "key",
        )
        .unwrap();
        let response = client
            .get_response(vec![Message::user("ping")], ChatOptions::default())
            .await
            .unwrap();
        server.await.unwrap();
        let usage = response.usage_details.expect("generateContent usage");
        assert_eq!(usage.input_token_count, Some(11));
        assert_eq!(usage.output_token_count, Some(6));
        assert_eq!(usage.reasoning_output_token_count, Some(2));
        assert_eq!(usage.cache_read_input_token_count, Some(5));

        let mut normalizer = StreamNormalizer::new(NativeProtocol::Gemini);
        let updates = [
            r#"{"candidates":[{"content":{"parts":[{"text":"a"}]}}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":2}}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":"b"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":6}}"#,
        ]
        .into_iter()
        .map(|event| normalizer.normalize(event.into()).unwrap().unwrap())
        .collect();
        let streamed = ChatResponse::from_updates(updates);
        assert_eq!(streamed.text(), "ab");
        let usage = streamed.usage_details.expect("streamed usage");
        assert_eq!(usage.input_token_count, Some(11));
        assert_eq!(usage.output_token_count, Some(6));
    }

    /// A delegated (Cloud IAM / OAuth) token expires mid-run. The client must
    /// re-resolve through the credential provider — which owns the cache and
    /// the refresh — rather than replay the token snapshotted into its header
    /// map when it was built, or every request after the first expiry is
    /// rejected for the rest of the process's life.
    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn delegated_tokens_refresh_per_request_instead_of_being_snapshotted() {
        use agent_framework_core::types::{ChatOptions, Message};
        use codypendent_providers::{CredentialError, DelegatedToken, TokenRequest};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, SystemTime};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        /// Every token is already inside the refresh skew, so each resolve
        /// mints a new one — the mid-run expiry this defect is about.
        struct ExpiringProvider(AtomicUsize);
        #[async_trait::async_trait]
        impl TokenProvider for ExpiringProvider {
            async fn token(
                &self,
                _: &TokenRequest,
            ) -> std::result::Result<DelegatedToken, CredentialError> {
                let nth = self.0.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(DelegatedToken::new(
                    format!("delegated-token-{nth}"),
                    SystemTime::now() + Duration::from_secs(5),
                ))
            }
        }

        // Two sequential requests over two connections, capturing both.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut captured = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0_u8; 4096];
                    let n = stream.read(&mut chunk).await.unwrap();
                    request.extend_from_slice(&chunk[..n]);
                    let head_end = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|position| position + 4);
                    let content_len = String::from_utf8_lossy(&request).lines().find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    });
                    if head_end.is_some_and(|head| request.len() >= head + content_len.unwrap_or(0))
                    {
                        break;
                    }
                }
                let body = r#"{"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                captured.push(String::from_utf8_lossy(&request).into_owned());
            }
            captured
        });

        let providers: codypendent_providers::ProvidersFile = toml::from_str(
            r#"
[[provider]]
id = "refreshing-gemini"
name = "Refreshing Gemini"
protocol = "gemini-native"
base_url = "https://unused.example/v1beta"
[[provider.auth]]
kind = "cloud_iam"
variant = "gcp_adc"
scopes = ["scope"]
"#,
        )
        .unwrap();
        let id = model_id("refreshing/model");
        let provider = Arc::new(ExpiringProvider(AtomicUsize::new(0)));
        let registry = ModelRegistry::new([ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".into(),
            base_url: format!("http://{address}/v1beta"),
            model: "test".into(),
            api_key_env: String::new(),
            context_tokens: None,
            provider_id: Some("refreshing-gemini".into()),
        }])
        .with_catalog(Catalog::from_providers(providers.providers))
        .with_token_provider("refreshing-gemini", provider.clone());

        // Built once — the token frozen here is `delegated-token-1`.
        let client = registry.client_for(&id).await.unwrap();
        for _ in 0..2 {
            client
                .get_response(vec![Message::user("x")], ChatOptions::default())
                .await
                .unwrap();
        }
        let captured = server.await.unwrap();
        let bearer = |request: &str| {
            request
                .to_lowercase()
                .lines()
                .find_map(|line| {
                    line.strip_prefix("authorization: bearer ")
                        .map(str::to_owned)
                })
                .expect("delegated bearer token on the wire")
        };
        let first = bearer(&captured[0]);
        let second = bearer(&captured[1]);
        assert_ne!(
            first, "delegated-token-1",
            "the expired construction-time token must not be replayed"
        );
        assert_ne!(first, second, "each request re-resolves the credential");
        assert!(provider.0.load(Ordering::SeqCst) >= 3);
    }

    /// The Anthropic Messages API accepts a STRING (or an array of content
    /// blocks) as `tool_result.content`, never an arbitrary JSON value, so a
    /// tool that returns an object or array must be serialized to text or the
    /// next model turn is rejected. Gemini takes the structured value as-is.
    #[cfg(feature = "provider-openai")]
    #[test]
    fn structured_tool_results_are_shaped_per_provider_contract() {
        use agent_framework_core::types::{
            ChatOptions, Content, FunctionArguments, FunctionCallContent, FunctionResultContent,
            Message, Role,
        };

        let cfg = |id: &str, provider_id: &str| ModelConfig {
            id: model_id(id),
            provider: "openai-compatible".into(),
            base_url: "http://localhost".into(),
            model: "test".into(),
            api_key_env: String::new(),
            provider_id: Some(provider_id.into()),
            context_tokens: None,
        };
        let messages = vec![
            Message::with_contents(
                Role::assistant(),
                vec![Content::FunctionCall(FunctionCallContent::new(
                    "call-1",
                    "lookup",
                    Some(FunctionArguments::Object(HashMap::new())),
                ))],
            ),
            Message::with_contents(
                Role::user(),
                vec![Content::FunctionResult(FunctionResultContent::new(
                    "call-1",
                    Some(serde_json::json!({"rows": [1, 2], "ok": true})),
                ))],
            ),
        ];

        let anthropic = NativeChatClient::new(
            &cfg("anthropic/results", "anthropic"),
            NativeProtocol::Anthropic,
            "key",
        )
        .unwrap();
        let body = anthropic.body(&messages, &ChatOptions::default()).unwrap();
        let result = &body["messages"][1]["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["tool_use_id"], "call-1");
        let content = result["content"]
            .as_str()
            .expect("tool_result content is a string, not a raw JSON value");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(content).unwrap(),
            serde_json::json!({"rows": [1, 2], "ok": true})
        );

        let gemini = NativeChatClient::new(
            &cfg("gemini/results", "gemini"),
            NativeProtocol::Gemini,
            "key",
        )
        .unwrap();
        let body = gemini.body(&messages, &ChatOptions::default()).unwrap();
        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"]["response"]["result"],
            serde_json::json!({"rows": [1, 2], "ok": true})
        );
    }

    /// Gemini's `functionResponse` needs BOTH the declared function `name` and
    /// the call `id`; when the model supplied its own id, the id must never be
    /// substituted for the name.
    #[cfg(feature = "provider-openai")]
    #[test]
    fn gemini_function_response_keeps_name_and_provider_id_distinct() {
        use agent_framework_core::types::{
            ChatOptions, Content, FunctionResultContent, Message, Role,
        };

        let client = NativeChatClient::new(
            &ModelConfig {
                id: model_id("gemini/named"),
                provider: "openai-compatible".into(),
                base_url: "http://localhost".into(),
                model: "gemini-test".into(),
                api_key_env: String::new(),
                provider_id: Some("gemini".into()),
                context_tokens: None,
            },
            NativeProtocol::Gemini,
            "key",
        )
        .unwrap();
        let calls = client
            .normalize(&serde_json::json!({
                "candidates": [{"content": {"parts": [
                    {"functionCall": {"id": "provider-call-77", "name": "lookup", "args": {}}}
                ]}}]
            }))
            .unwrap();
        assert_eq!(
            calls[0].as_function_call().unwrap().call_id,
            "provider-call-77"
        );

        let messages = vec![
            Message::with_contents(Role::assistant(), calls),
            Message::with_contents(
                Role::user(),
                vec![Content::FunctionResult(FunctionResultContent::new(
                    "provider-call-77",
                    Some(serde_json::json!("done")),
                ))],
            ),
        ];
        let body = client.body(&messages, &ChatOptions::default()).unwrap();
        let response = &body["contents"][1]["parts"][0]["functionResponse"];
        assert_eq!(response["name"], "lookup");
        assert_eq!(response["id"], "provider-call-77");
    }

    #[cfg(feature = "provider-openai")]
    #[test]
    fn gemini_parallel_same_name_calls_roundtrip_with_unique_synthetic_ids() {
        use agent_framework_core::types::{
            ChatOptions, Content, FunctionResultContent, Message, Role,
        };

        let cfg = ModelConfig {
            id: model_id("gemini/roundtrip"),
            provider: "openai-compatible".into(),
            base_url: "http://localhost".into(),
            model: "gemini-test".into(),
            api_key_env: String::new(),
            provider_id: Some("gemini".into()),
            context_tokens: None,
        };
        let client = NativeChatClient::new(&cfg, NativeProtocol::Gemini, "key").unwrap();
        let response = serde_json::json!({
            "candidates": [{"content": {"parts": [
                {"functionCall": {"name": "lookup", "args": {"item": 1}}},
                {"functionCall": {"name": "lookup", "args": {"item": 2}}}
            ]}}]
        });
        let calls = client.normalize(&response).unwrap();
        let first = calls[0].as_function_call().unwrap();
        let second = calls[1].as_function_call().unwrap();
        assert_ne!(first.call_id, second.call_id);
        let first_id = first.call_id.clone();
        let second_id = second.call_id.clone();

        let messages = vec![
            Message::with_contents(Role::assistant(), calls),
            Message::with_contents(
                Role::user(),
                vec![
                    Content::FunctionResult(FunctionResultContent::new(
                        first_id.clone(),
                        Some(serde_json::json!("one")),
                    )),
                    Content::FunctionResult(FunctionResultContent::new(
                        second_id.clone(),
                        Some(serde_json::json!("two")),
                    )),
                ],
            ),
        ];
        let request = client.body(&messages, &ChatOptions::default()).unwrap();
        let responses = request["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(responses[0]["functionResponse"]["name"], "lookup");
        assert_eq!(responses[1]["functionResponse"]["name"], "lookup");
        assert_eq!(responses[0]["functionResponse"]["id"], first_id);
        assert_eq!(responses[1]["functionResponse"]["id"], second_id);
    }

    #[cfg(feature = "provider-openai")]
    #[test]
    fn gemini_synthetic_ids_remain_unique_across_retained_turns() {
        use agent_framework_core::types::{
            ChatOptions, Content, FunctionResultContent, Message, Role,
        };

        let cfg = ModelConfig {
            id: model_id("gemini/history"),
            provider: "openai-compatible".into(),
            base_url: "http://localhost".into(),
            model: "gemini-test".into(),
            api_key_env: String::new(),
            provider_id: Some("gemini".into()),
            context_tokens: None,
        };
        let first_client = NativeChatClient::new(&cfg, NativeProtocol::Gemini, "key").unwrap();
        let response = |name: &str| {
            serde_json::json!({
                "candidates": [{"content": {"parts": [
                    {"functionCall": {"name": name, "args": {}}}
                ]}}]
            })
        };
        let first_call = first_client.normalize(&response("first")).unwrap();
        // Retained history may be resumed by a newly constructed client after
        // a later run or process restart.
        let resumed_client = NativeChatClient::new(&cfg, NativeProtocol::Gemini, "key").unwrap();
        let second_call = resumed_client.normalize(&response("second")).unwrap();
        let first_id = first_call[0].as_function_call().unwrap().call_id.clone();
        let second_id = second_call[0].as_function_call().unwrap().call_id.clone();
        assert_ne!(first_id, second_id);
        for id in [&first_id, &second_id] {
            let uuid = uuid::Uuid::parse_str(
                id.strip_prefix("gemini-synthetic-")
                    .expect("synthetic ID prefix"),
            )
            .expect("synthetic ID UUID");
            assert_eq!(uuid.get_version_num(), 7);
        }

        let messages = vec![
            Message::with_contents(Role::assistant(), first_call),
            Message::with_contents(
                Role::user(),
                vec![Content::FunctionResult(FunctionResultContent::new(
                    first_id,
                    Some(serde_json::json!("one")),
                ))],
            ),
            Message::with_contents(Role::assistant(), second_call),
            Message::with_contents(
                Role::user(),
                vec![Content::FunctionResult(FunctionResultContent::new(
                    second_id,
                    Some(serde_json::json!("two")),
                ))],
            ),
        ];
        let request = resumed_client
            .body(&messages, &ChatOptions::default())
            .unwrap();
        assert_eq!(
            request["contents"][1]["parts"][0]["functionResponse"]["name"],
            "first"
        );
        assert_eq!(
            request["contents"][3]["parts"][0]["functionResponse"]["name"],
            "second"
        );
    }

    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn bounded_response_redacts_before_public_truncation() {
        const LIMIT: usize = 32;
        let cases = [
            ("raw-secret-value", vec!["raw-secret-value".to_owned()]),
            (
                "Bearer rendered-secret-value",
                vec![
                    "Bearer rendered-secret-value".to_owned(),
                    "rendered-secret-value".to_owned(),
                ],
            ),
        ];
        for (secret, secrets) in cases {
            let body = format!("{}{secret} trailing", "x".repeat(LIMIT - 1));
            let (url, server) = native_capture_server("200 OK", "text/plain", body).await;
            let response = reqwest::get(url).await.unwrap();
            let snippet = bounded_response(response, LIMIT, &secrets).await.unwrap();
            server.await.unwrap();
            assert!(!snippet.contains(secret));
            assert!(!snippet.contains(&secret[..secret.len().min(8)]));
            assert!(snippet.ends_with("… [truncated]"));
        }

        let body = "x".repeat(LIMIT);
        let (url, server) = native_capture_server("200 OK", "text/plain", body.clone()).await;
        let response = reqwest::get(url).await.unwrap();
        let snippet = bounded_response(response, LIMIT, &[]).await.unwrap();
        server.await.unwrap();
        assert_eq!(snippet, body);
    }

    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn bytewise_sse_done_and_errors_are_terminal() {
        use agent_framework_core::client::ChatClient;
        use agent_framework_core::types::{ChatOptions, Message};
        use futures::StreamExt;
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}\r\n\r\n",
            "data: {not json}\r\n\r\n"
        )
        .to_owned();
        let (base_url, server) = bytewise_native_server(body).await;
        let client = NativeChatClient::new(
            &ModelConfig {
                id: model_id("gemini/bytewise"),
                provider: "openai-compatible".into(),
                base_url,
                model: "test".into(),
                api_key_env: String::new(),
                context_tokens: None,
                provider_id: Some("gemini".into()),
            },
            NativeProtocol::Gemini,
            "key",
        )
        .unwrap();
        let updates = client
            .get_streaming_response(vec![Message::user("x")], ChatOptions::default())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        server.await.unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].as_ref().unwrap().text_content(), "ok");
    }

    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn native_streams_reject_premature_eof() {
        use agent_framework_core::client::ChatClient;
        use agent_framework_core::types::{ChatOptions, Message};
        use futures::StreamExt;

        for (protocol, body) in [
            (
                NativeProtocol::Anthropic,
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"cut\"}}\n\n",
            ),
            (
                NativeProtocol::Gemini,
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"cut\"}]}}]}\n\n",
            ),
        ] {
            let (base_url, server) = bytewise_native_server(body.to_owned()).await;
            let base_url = if matches!(protocol, NativeProtocol::Anthropic) {
                base_url.trim_end_matches("/v1beta").to_owned()
            } else {
                base_url
            };
            let client = NativeChatClient::new(
                &ModelConfig {
                    id: model_id("native/cutoff"),
                    provider: "openai-compatible".into(),
                    base_url,
                    model: "test".into(),
                    api_key_env: String::new(),
                    context_tokens: None,
                    provider_id: None,
                },
                protocol,
                "key",
            )
            .unwrap();
            let updates = client
                .get_streaming_response(vec![Message::user("x")], ChatOptions::default())
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            server.await.unwrap();
            assert_eq!(updates.len(), 2);
            assert_eq!(updates[0].as_ref().unwrap().text_content(), "cut");
            assert!(updates[1]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("terminal event"));
        }
    }

    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn delegated_credentials_are_persistent_single_flight_and_sent_as_bearer() {
        use agent_framework_core::types::{ChatOptions, Message};
        use codypendent_providers::{CredentialError, DelegatedToken, TokenRequest};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, SystemTime};

        struct CountingProvider(AtomicUsize);
        #[async_trait::async_trait]
        impl TokenProvider for CountingProvider {
            async fn token(
                &self,
                _: &TokenRequest,
            ) -> std::result::Result<DelegatedToken, CredentialError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                Ok(DelegatedToken::new(
                    "delegated-secret",
                    SystemTime::now() + Duration::from_secs(3600),
                ))
            }
        }

        let providers: codypendent_providers::ProvidersFile = toml::from_str(
            r#"
[[provider]]
id = "delegated-gemini"
name = "Delegated Gemini"
protocol = "gemini-native"
base_url = "https://unused.example/v1beta"
extra_headers = { "authorization" = "Catalog must not win", "x-delegated-extra" = "yes" }
query_params = { "tenant" = "delegated user" }
[[provider.auth]]
kind = "cloud_iam"
variant = "gcp_adc"
scopes = ["scope"]
"#,
        )
        .unwrap();
        let response =
            r#"{"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}]}"#
                .to_owned();
        let (base_url, server) =
            native_capture_server("200 OK", "application/json", response).await;
        let id = model_id("delegated/model");
        let provider = Arc::new(CountingProvider(AtomicUsize::new(0)));
        let registry = ModelRegistry::new([ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".into(),
            base_url,
            model: "test".into(),
            api_key_env: String::new(),
            context_tokens: None,
            provider_id: Some("delegated-gemini".into()),
        }])
        .with_catalog(Catalog::from_providers(providers.providers))
        .with_token_provider("delegated-gemini", provider.clone());

        let clients = futures::future::join_all((0..8).map(|_| registry.client_for(&id))).await;
        assert!(clients.iter().all(Result::is_ok));
        assert_eq!(provider.0.load(Ordering::SeqCst), 1);
        clients
            .into_iter()
            .next()
            .unwrap()
            .unwrap()
            .get_response(vec![Message::user("x")], ChatOptions::default())
            .await
            .unwrap();
        let request = server.await.unwrap().to_lowercase();
        assert!(request.contains("authorization: bearer delegated-secret"));
        assert!(!request.contains("authorization: catalog must not win"));
        assert!(request.contains("x-delegated-extra: yes"));
        assert!(
            request.starts_with("post /v1beta/models/test:generatecontent?tenant=delegated+user ")
        );
    }

    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn delegated_readiness_check_and_client_share_one_bearer_token() {
        use codypendent_providers::{CredentialError, DelegatedToken, TokenRequest};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, SystemTime};

        struct CountingProvider(AtomicUsize);
        #[async_trait::async_trait]
        impl TokenProvider for CountingProvider {
            async fn token(
                &self,
                _: &TokenRequest,
            ) -> std::result::Result<DelegatedToken, CredentialError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(DelegatedToken::new(
                    "readiness-secret",
                    SystemTime::now() + Duration::from_secs(3600),
                ))
            }
        }

        let providers: codypendent_providers::ProvidersFile = toml::from_str(
            r#"
[[provider]]
id = "readiness-gemini"
name = "Readiness Gemini"
protocol = "gemini-native"
base_url = "https://unused.example/v1beta"
[[provider.auth]]
kind = "cloud_iam"
variant = "gcp_adc"
scopes = ["scope"]
"#,
        )
        .unwrap();
        let (base_url, server) =
            capture_server(r#"{"models":[{"name":"models/gemini-test"}]}"#).await;
        let id = model_id("readiness/gemini-test");
        let provider = Arc::new(CountingProvider(AtomicUsize::new(0)));
        let registry = ModelRegistry::new([ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".into(),
            base_url,
            model: "gemini-test".into(),
            api_key_env: String::new(),
            context_tokens: None,
            provider_id: Some("readiness-gemini".into()),
        }])
        .with_catalog(Catalog::from_providers(providers.providers))
        .with_token_provider("readiness-gemini", provider.clone());

        assert!(registry.credentials_resolvable(&id).await.unwrap());
        registry.check_model(&id).await.unwrap();
        let request = server.await.unwrap().to_lowercase();
        assert!(request.contains("authorization: bearer readiness-secret"));
        registry.client_for(&id).await.unwrap();
        assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn delegated_readiness_without_injection_fails_without_leaking_secrets() {
        let providers: codypendent_providers::ProvidersFile = toml::from_str(
            r#"
[[provider]]
id = "missing-delegated"
name = "Missing delegated"
protocol = "gemini-native"
base_url = "https://unused.example/v1beta"
[[provider.auth]]
kind = "o_auth"
authorize_url = "https://auth.example/authorize"
token_url = "https://auth.example/token"
client_id = "public-client-id"
scopes = ["scope"]
"#,
        )
        .unwrap();
        let id = model_id("missing/model");
        let registry = ModelRegistry::new([ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".into(),
            base_url: "https://unused.example/v1beta".into(),
            model: "model".into(),
            api_key_env: String::new(),
            context_tokens: None,
            provider_id: Some("missing-delegated".into()),
        }])
        .with_catalog(Catalog::from_providers(providers.providers));

        let error = registry.credentials_resolvable(&id).await.unwrap_err();
        let display = error.to_string();
        assert!(display.contains("requires an injected token provider"));
        assert!(!display.contains("public-client-id"));
    }

    /// Same F3 fix, the reachability-probe half: `check_model` must ask
    /// Anthropic's real `GET /v1/models` (spec-fixed path; verified against
    /// `platform.claude.com/docs/en/api/models/list`), not the OpenAI-chat
    /// `{base_url}/models` this build used to send unconditionally — which
    /// resolved to `https://api.anthropic.com/models`, a route that does not
    /// exist, so `codypendent models check` on a freshly `models add`ed
    /// Anthropic entry always failed even with a valid key.
    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn check_model_asks_v1_models_for_the_anthropic_protocol() {
        use crate::auth::AuthStore;

        let provider_toml = r#"
[[provider]]
id = "anthropic"
name = "Anthropic (Claude)"
protocol = "anthropic"
base_url = "https://unused.example"
extra_headers = { "anthropic-version" = "2099-01-01", "x-check-extra" = "yes" }
query_params = { "scope" = "model list", "reserved" = "x/y" }
[[provider.auth]]
kind = "api_key"
env = ["ANTHROPIC_API_KEY_UNSET_TEST_2"]
header = "x-check-key"
prefix = "Check "
"#;
        let file: codypendent_providers::ProvidersFile =
            toml::from_str(provider_toml).expect("provider toml");
        let catalog = Catalog::from_providers(file.providers);

        let (url, server) = capture_server(r#"{"data":[{"id":"claude-opus-5"}]}"#).await;
        let base_url = url.trim_end_matches("/v1").to_string();

        let id = model_id("anthropic/claude-opus-5");
        let mut auth = AuthStore::default();
        auth.set(id.0.as_str(), "sk-ant-secret");
        let registry = ModelRegistry::new([ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".to_string(),
            base_url,
            model: "claude-opus-5".to_string(),
            api_key_env: String::new(),
            context_tokens: None,
            provider_id: Some("anthropic".to_string()),
        }])
        .with_auth(auth)
        .with_catalog(catalog);

        registry
            .check_model(&id)
            .await
            .expect("the endpoint lists the model at /v1/models");
        let request = server.await.unwrap();
        assert!(
            request
                .to_lowercase()
                .starts_with("get /v1/models?reserved=x%2fy&scope=model+list "),
            "must probe the spec-fixed Anthropic Models API path:\n{request}"
        );
        let lower = request.to_lowercase();
        assert!(lower.contains("x-check-key: check sk-ant-secret"));
        assert!(lower.contains("x-check-extra: yes"));
        assert!(lower.contains("anthropic-version: 2099-01-01"));
        assert!(!lower.contains("x-api-key:"));
    }

    /// A stored provider-wide key (`provider/<id>`) satisfies a model that has
    /// no per-model `auth.json` entry and no env var — the dedupe-key-entry
    /// seam: one pasted key serves every model added from that provider.
    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn provider_wide_auth_json_key_resolves_for_models_of_that_provider() {
        use crate::auth::AuthStore;
        let id = model_id("nebius/deepseek");
        let mut auth = AuthStore::default();
        auth.set(provider_auth_id("nebius"), "nb-provider-key");
        let registry = ModelRegistry::new([ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".to_string(),
            base_url: "https://api.tokenfactory.nebius.com/v1".to_string(),
            model: "deepseek-ai/DeepSeek-V4-Flash".to_string(),
            api_key_env: String::new(),
            context_tokens: None,
            provider_id: Some("nebius".to_string()),
        }])
        .with_auth(auth);
        assert!(
            registry.client_for(&id).await.is_ok(),
            "a provider-wide stored key must satisfy the model"
        );
    }

    /// The discovery readiness check and the live client both delegate to
    /// `api_key_for`; pin the complete precedence here so neither side can
    /// silently drift as more credential sources are added.
    #[cfg(feature = "provider-openai")]
    #[tokio::test]
    async fn model_credentials_follow_model_provider_explicit_and_catalog_env_precedence() {
        use crate::auth::AuthStore;

        const MODEL_ENV: &str = "CODYPENDENT_TEST_MODEL_PRECEDENCE_KEY_17d9";
        const PROVIDER_ENV: &str = "CODYPENDENT_TEST_PROVIDER_PRECEDENCE_KEY_17d9";
        std::env::set_var(MODEL_ENV, "from-model-env");
        std::env::set_var(PROVIDER_ENV, "from-provider-env");

        let provider_toml = format!(
            r#"
[[provider]]
id = "precedence-test"
name = "Precedence test"
protocol = "openai-chat"
base_url = "https://example.invalid/v1"
[[provider.auth]]
kind = "api_key"
env = ["{PROVIDER_ENV}"]
header = "Authorization"
prefix = "Bearer "
"#
        );
        let file: codypendent_providers::ProvidersFile =
            toml::from_str(&provider_toml).expect("provider toml");
        let catalog = Catalog::from_providers(file.providers);
        let id = model_id("precedence/model");
        let cfg = ModelConfig {
            id: id.clone(),
            provider: "openai-compatible".to_string(),
            base_url: "https://example.invalid/v1".to_string(),
            model: "model".to_string(),
            api_key_env: MODEL_ENV.to_string(),
            provider_id: Some("precedence-test".to_string()),
            context_tokens: None,
        };

        let mut auth = AuthStore::default();
        auth.set(id.0.as_str(), "from-model-auth");
        auth.set(provider_auth_id("precedence-test"), "from-provider-auth");
        let registry = ModelRegistry::new([cfg.clone()])
            .with_auth(auth)
            .with_catalog(catalog.clone());
        assert_eq!(
            registry.api_key_for(&cfg).await.expect("model auth"),
            "from-model-auth"
        );

        let mut auth = AuthStore::default();
        auth.set(provider_auth_id("precedence-test"), "from-provider-auth");
        let registry = ModelRegistry::new([cfg.clone()])
            .with_auth(auth)
            .with_catalog(catalog.clone());
        assert_eq!(
            registry.api_key_for(&cfg).await.expect("provider auth"),
            "from-provider-auth"
        );

        let registry = ModelRegistry::new([cfg.clone()]).with_catalog(catalog.clone());
        assert_eq!(
            registry.api_key_for(&cfg).await.expect("model env"),
            "from-model-env"
        );

        let provider_env_cfg = ModelConfig {
            api_key_env: String::new(),
            ..cfg.clone()
        };
        let registry = ModelRegistry::new([provider_env_cfg.clone()]).with_catalog(catalog.clone());
        assert_eq!(
            registry
                .api_key_for(&provider_env_cfg)
                .await
                .expect("provider env"),
            "from-provider-env"
        );
        assert!(registry
            .credentials_resolvable(&id)
            .await
            .expect("readiness uses the same resolver"));

        // An explicit model env is authoritative: if it is missing, do not
        // silently switch to the provider env and run with a different key.
        let missing_explicit = ModelConfig {
            api_key_env: "CODYPENDENT_TEST_EXPLICIT_MISSING_KEY_17d9".to_string(),
            ..cfg
        };
        let registry = ModelRegistry::new([missing_explicit.clone()]).with_catalog(catalog);
        assert!(matches!(
            registry.api_key_for(&missing_explicit).await,
            Err(ModelsError::MissingApiKeyEnv { .. })
        ));

        std::env::remove_var(MODEL_ENV);
        std::env::remove_var(PROVIDER_ENV);
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
