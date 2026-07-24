# Universal Model Providers + ACP (Foundation) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the runtime's single hard-coded `"openai-compatible"` provider into a data-driven `Provider`/`Model`/`Protocol`/`AuthMethod` abstraction with a credential-provider trait, a curated ~40-provider catalog, generalized model-client construction, an ACP *client* that delegates a run to an external agent subprocess, and a TUI provider picker — all a strict, backward-compatible superset of today's `models.toml`.

**Architecture:** A new leaf crate `crates/providers` holds the provider/auth data model, the async `CredentialProvider` trait (`ApiKey` wired; `CloudIam`/`OAuth` trait-shaped stubs), and the built-in catalog (embedded TOML, shadowable by a user `providers.toml`). `crates/runtime`'s `ModelRegistry::client_for` is generalized to build an `Arc<dyn ChatClient>` by dispatching on `Protocol` and resolving a `CredentialProvider` — today's `ModelConfig` maps to `(OpenAiChat, [ApiKey])` so every existing config keeps working. A new `crates/integrations/src/acp_client.rs` (the *client* role — the existing `acp.rs` is the *server* role) spawns an ACP agent, does the initialize/session handshake, delegates the objective as an ACP prompt, and maps the agent's streamed `session/update`s onto the existing `EventBody` model. The TUI gains a `/provider` picker mirroring the merged `/model` picker.

**Tech Stack:** Rust (edition 2021, rust-version 1.82); `serde`/`toml`; `async-trait`; `agent-framework-core` `ChatClient` + `agent-framework-openai` (feature `provider-openai`); the new `agent-client-protocol` crate (ACP reference impl, JSON-RPC 2.0 over stdio); ratatui pure-reducer TUI; `tokio`.

## Global Constraints

- **Secrets referenced by env-var NAME, never persisted.** No catalog, config, or credential type stores a secret value; the value is read from the environment at call time and never logged or placed in model context. (`crates/providers` stores only env-var names.)
- **Additive protocol; existing `models.toml` keeps working.** No new wire event type; ACP updates are translated daemon-side onto the existing `EventBody` variants. An existing `models.toml` (`provider = "openai-compatible"`) parses and runs unchanged.
- **Preserve the routing classification hard-filter.** A hosted provider stays gated by data classification — the executor's `routing.validate_pin` / `routing.select` path (`crates/codypendentd/src/executor.rs:355-427`) is untouched; the catalog/picker only *browses/stages*, never bypasses routing.
- **T1/T7 cost honesty.** `ModelUsage.cost_micros` stays `None` for the live driver (tokens measured, cost applied downstream). Any catalog cost metadata is display-only and is **never** summed into a budget.
- **`agent-client-protocol` is the only new external dependency.** Vet its licence under the existing `cargo deny` gate (`deny.toml`); its transitive deps must satisfy the licence allow-list or be added with justification.
- Clippy runs on Linux CI: `cargo clippy --workspace --all-targets --all-features -- -D warnings`. Gate any platform-only helper with the same `#[cfg]` as its sole caller.
- NEVER edit/stage `README.md`, `docs/cli-and-tui-user-guide.md`, `docs/docs/*`, `ROADMAP.md`, or anything under `.superpowers/`. Stage only changed files by explicit path; never `git add -A`.
- Commit trailer on **every** commit: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Full gate green per task: `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace --all-features`.

---

## Design decisions

**Where the abstraction/catalog lives — a new `crates/providers` leaf crate.** Justification:

- It is protocol-adjacent *pure data* (catalog + auth shapes) plus one small async credential trait — no agent-framework, no network, no daemon. This mirrors `crates/routing`'s "daemon-free, no network calls, leaf crate" design (`crates/routing/src/lib.rs:26-29`).
- It is consumed by **two** unrelated crates: `crates/runtime` (to build model clients) and `crates/cli` (to seed the TUI provider-picker projection). Putting it in `runtime` would force the `cli` picker to pull the whole runtime (agent-framework) just to read a catalog; putting it in `routing` would conflate provider identity/auth with *measured* routing. A focused leaf crate keeps both dependents thin.
- It introduces **no new external dependency** (only `serde`, `toml`, `thiserror`, `async-trait`, all already in `[workspace.dependencies]`).

**Where the ACP *client* lives — `crates/integrations/src/acp_client.rs`.** The existing `crates/integrations/src/acp.rs` is the **server/agent** role (Codypendent serves ACP to Zed as the client; see its module doc "the *agent* is the server ... Zed is the *client*"). This spec's ACP work is the **opposite role** — Codypendent as the *client/host* connecting to an external agent. It belongs in the crate that already owns ACP, as a sibling module, reusing its `StopReason`/`PermissionOption`/`PermissionOutcome` types. `crates/integrations` already depends on `codypendent-protocol` (so it can emit `EventBody`) and does not depend on the daemon/runtime — the ACP client stays a testable leaf. It gains the new `agent-client-protocol` dependency.

**Async `CredentialProvider`.** The trait is `async` (per the approved design) so `CloudIam`/`OAuth` follow-ups (token refresh, request signing) slot in *without another seam change*. `ApiKey` resolves synchronously inside its `async fn`. This ripples `ModelRegistry::client_for` and `FrameworkModelDriver::from_registry` to `async` and adds one `.await` at each of the two production call sites (executor + workflow factory) — mechanical and contained (Task 4).

## File structure

- **New:** `crates/providers/Cargo.toml`, `crates/providers/src/lib.rs`, `crates/providers/src/model.rs` (types), `crates/providers/src/credential.rs` (trait + impls), `crates/providers/src/catalog.rs` (loader), `crates/providers/builtin_catalog.toml` (embedded data) — Tasks 1-3.
- **Modify:** root `Cargo.toml` (workspace members + deps) — Tasks 1, 5.
- **Modify:** `crates/runtime/Cargo.toml`, `crates/runtime/src/models.rs`, `crates/runtime/src/agent.rs` — Task 4 (generalize client construction).
- **Modify:** `crates/codypendentd/src/executor.rs:430`, `crates/codypendentd/src/workflow_exec.rs:355` — Task 4 (`.await` the now-async builder).
- **New:** `crates/integrations/src/acp_client.rs`; **modify** `crates/integrations/src/lib.rs`, `crates/integrations/Cargo.toml` — Tasks 5-7 (ACP client).
- **Modify:** `crates/tui/src/palette.rs`, `crates/tui/src/state.rs`, `crates/tui/src/reduce.rs`, `crates/cli/src/tui.rs` — Task 8 (provider picker).

---

## Task 1: `crates/providers` crate — the provider/auth data model

Create the leaf crate and its `Provider`/`Model`/`Protocol`/`AuthMethod` types — the backward-compatible superset of `models.toml`, parseable from TOML.

**Files:**
- Create: `crates/providers/Cargo.toml`
- Create: `crates/providers/src/lib.rs`
- Create: `crates/providers/src/model.rs`
- Modify: `Cargo.toml` (root — add the member + workspace dep)
- Test: `crates/providers/src/model.rs` `#[cfg(test)]`

**Interfaces:**
- Produces: `Protocol { OpenAiChat, Anthropic, GeminiNative, Acp }` (`#[non_exhaustive]`, serde kebab-case); `AuthMethod { None, ApiKey{env,header,prefix}, CloudIam{variant,env,scopes}, Acp{command,args,env}, OAuth{authorize_url,token_url,client_id,scopes,pkce} }` (internally tagged `kind`, snake_case, `#[non_exhaustive]`); `Provider { id, name, protocol, base_url: Option<String>, auth: Vec<AuthMethod>, extra_headers: BTreeMap, query_params: BTreeMap, local: bool }`; `Model { id, provider_id, name: Option, context_tokens: Option, cost_per_1m_input_usd: Option, cost_per_1m_output_usd: Option }`.

- [ ] **Step 1: Add the crate to the workspace**

In the root `Cargo.toml`, add `"crates/providers",` to `[workspace] members` (alongside `"crates/routing",` at line 15) and add to `[workspace.dependencies]` (after `codypendent-routing` at line 41):

```toml
codypendent-providers = { path = "crates/providers" }
```

Create `crates/providers/Cargo.toml`:

```toml
[package]
name = "codypendent-providers"
description = "codypendent-providers: the provider/auth data model, credential-provider trait, and curated built-in catalog. A daemon-free, network-free leaf crate (like codypendent-routing); no agent-framework dependency."
version.workspace = true
edition.workspace = true
rust-version.workspace = true
repository.workspace = true
license.workspace = true

[dependencies]
serde = { workspace = true }
toml = { workspace = true }
thiserror = { workspace = true }
async-trait = { workspace = true }

[dev-dependencies]
tokio = { workspace = true }

[lints]
workspace = true
```

- [ ] **Step 2: Write the failing test** in a new `crates/providers/src/model.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_openai_compatible_provider_from_toml() {
        let text = r#"
[[provider]]
id = "groq"
name = "Groq"
protocol = "openai-chat"
base_url = "https://api.groq.com/openai/v1"
[[provider.auth]]
kind = "api_key"
env = ["GROQ_API_KEY"]
"#;
        let file: ProvidersFile = toml::from_str(text).expect("parse providers toml");
        assert_eq!(file.providers.len(), 1);
        let p = &file.providers[0];
        assert_eq!(p.id, "groq");
        assert_eq!(p.protocol, Protocol::OpenAiChat);
        assert_eq!(p.base_url.as_deref(), Some("https://api.groq.com/openai/v1"));
        assert!(!p.local);
        // Header/prefix default to Authorization / "Bearer ".
        match &p.auth[0] {
            AuthMethod::ApiKey { env, header, prefix } => {
                assert_eq!(env, &vec!["GROQ_API_KEY".to_string()]);
                assert_eq!(header, "Authorization");
                assert_eq!(prefix, "Bearer ");
            }
            other => panic!("expected ApiKey, got {other:?}"),
        }
    }

    #[test]
    fn parses_anthropic_native_and_acp_variants() {
        let text = r#"
[[provider]]
id = "anthropic"
name = "Anthropic"
protocol = "anthropic"
base_url = "https://api.anthropic.com"
extra_headers = { "anthropic-version" = "2023-06-01" }
[[provider.auth]]
kind = "api_key"
env = ["ANTHROPIC_API_KEY"]
header = "x-api-key"
prefix = ""

[[provider]]
id = "claude-code"
name = "Claude Code (ACP)"
protocol = "acp"
[[provider.auth]]
kind = "acp"
command = "npx"
args = ["-y", "@agentclientprotocol/claude-agent-acp"]
"#;
        let file: ProvidersFile = toml::from_str(text).expect("parse");
        assert_eq!(file.providers[0].protocol, Protocol::Anthropic);
        assert_eq!(
            file.providers[0].extra_headers.get("anthropic-version").map(String::as_str),
            Some("2023-06-01")
        );
        match &file.providers[0].auth[0] {
            AuthMethod::ApiKey { header, prefix, .. } => {
                assert_eq!(header, "x-api-key");
                assert_eq!(prefix, "");
            }
            other => panic!("expected ApiKey, got {other:?}"),
        }
        assert_eq!(file.providers[1].protocol, Protocol::Acp);
        match &file.providers[1].auth[0] {
            AuthMethod::Acp { command, args, .. } => {
                assert_eq!(command, "npx");
                assert_eq!(args, &vec!["-y".to_string(), "@agentclientprotocol/claude-agent-acp".to_string()]);
            }
            other => panic!("expected Acp, got {other:?}"),
        }
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p codypendent-providers parses_an_openai_compatible_provider_from_toml`
Expected: FAIL to compile — the crate/types do not exist yet.

- [ ] **Step 4: Write the types** — the rest of `crates/providers/src/model.rs`:

```rust
//! The provider/auth data model: a backward-compatible superset of `models.toml`.
//!
//! Everything here is pure data — no secrets, no network. A secret is referenced
//! by env-var NAME only (`AuthMethod::ApiKey.env`); its value is read at call
//! time by a [`crate::credential::CredentialProvider`] and never stored here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The on-disk shape of a `providers.toml`: a bare array of `[[provider]]` tables.
#[derive(Debug, Default, Deserialize)]
pub struct ProvidersFile {
    #[serde(default, rename = "provider")]
    pub providers: Vec<Provider>,
    #[serde(default, rename = "model")]
    pub models: Vec<Model>,
}

/// The wire protocol a provider speaks. EXPLICIT (never inferred) — the research
/// found every prior-art catalog that leaves it implicit pays for it. Extensible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Protocol {
    /// `POST {base}/chat/completions` — today's only wired case.
    OpenAiChat,
    /// Anthropic Messages (`POST {base}/v1/messages` + `anthropic-version`). Catalog-only this PR.
    Anthropic,
    /// Gemini/Vertex `:generateContent`. Catalog-only this PR.
    GeminiNative,
    /// Not HTTP: a spawned agent subprocess, JSON-RPC 2.0 over stdio (see `acp_client`).
    Acp,
}

/// How a provider authenticates. `#[serde(tag = "kind")]` so each `[[provider.auth]]`
/// table names its `kind`. A `Vec<AuthMethod>` on a provider expresses "paste a key
/// OR log in" (Azure/Bedrock/Anthropic legitimately offer several).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AuthMethod {
    /// No auth (local endpoints: Ollama / LM Studio / vLLM).
    None,
    /// A static API key: the first `env` NAME that is set wins; injected under
    /// `header` with `prefix`. The value is NEVER stored — only the NAME is here.
    ApiKey {
        env: Vec<String>,
        #[serde(default = "default_auth_header")]
        header: String,
        #[serde(default = "default_auth_prefix")]
        prefix: String,
    },
    /// Cloud IAM (AWS SigV4 / GCP ADC / Azure Entra) — TRAIT-SHAPED STUB this PR
    /// (env-var NAMEs only; signing/refresh is a follow-up).
    CloudIam {
        /// `"aws_sigv4"` | `"gcp_adc"` | `"azure_entra"`.
        variant: String,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        scopes: Vec<String>,
    },
    /// An ACP agent subprocess launch line (the model is the agent's, not ours).
    Acp {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    /// Subscription OAuth — RESERVED, opt-in, ToS-gated, NOT wired (no
    /// reverse-engineered flow ships; see the design's ToS decision).
    OAuth {
        authorize_url: String,
        token_url: String,
        client_id: String,
        #[serde(default)]
        scopes: Vec<String>,
        #[serde(default)]
        pkce: bool,
    },
}

fn default_auth_header() -> String {
    "Authorization".to_string()
}
fn default_auth_prefix() -> String {
    "Bearer ".to_string()
}

/// One callable endpoint family. `(id, base_url, protocol, auth)` is everything a
/// client needs to reach it; the secret is referenced by NAME inside `auth`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub auth: Vec<AuthMethod>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub query_params: BTreeMap<String, String>,
    /// On-device (Ollama/LM Studio/vLLM). The routing hard filter treats local
    /// providers as able to process any data classification; hosted ones are gated.
    #[serde(default)]
    pub local: bool,
}

/// A catalog model row, addressed by `(provider_id, id)`. All metadata is optional
/// and DISPLAY-ONLY — cost fields are never summed into a budget (T1/T7 honesty).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_per_1m_input_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_per_1m_output_usd: Option<f64>,
}
```

Create `crates/providers/src/lib.rs`:

```rust
//! codypendent-providers — the provider/auth data model, credential-provider
//! trait, and curated built-in catalog. A daemon-free, network-free leaf crate.

pub mod catalog;
pub mod credential;
pub mod model;

pub use catalog::{builtin_providers, Catalog, CatalogError};
pub use credential::{credential_for, CredentialError, CredentialProvider, ResolvedCredential};
pub use model::{AuthMethod, Model, Protocol, Provider, ProvidersFile};
```

(The `catalog` and `credential` modules are added in Tasks 2-3; to compile this task in isolation, temporarily comment their `pub mod`/`pub use` lines, or implement Tasks 1-3 back-to-back before the first `cargo test --workspace`. Prefer the latter — they are one crate.)

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p codypendent-providers parses_`
Expected: PASS (both parse tests).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all -- --check && cargo clippy -p codypendent-providers --all-targets -- -D warnings
git add Cargo.toml crates/providers/Cargo.toml crates/providers/src/lib.rs crates/providers/src/model.rs
git commit -m "feat(providers): provider/auth data model (Provider/Model/Protocol/AuthMethod)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 2: `CredentialProvider` trait + `ApiKey` impl (+ CloudIam/OAuth stubs)

The async credential seam: resolve auth material from the environment at call time (never stored). `ApiKey` is wired; `CloudIam`/`OAuth` are trait-shaped stubs that error `NotWired`.

**Files:**
- Create: `crates/providers/src/credential.rs`
- Test: `crates/providers/src/credential.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `AuthMethod` (Task 1).
- Produces: `#[async_trait] trait CredentialProvider { async fn resolve(&self) -> Result<ResolvedCredential, CredentialError>; }`; `ResolvedCredential { None, ApiKey { header, prefix, value } }`; `CredentialError { MissingEnv { var }, NotWired { method } }`; `fn credential_for(&AuthMethod) -> Box<dyn CredentialProvider>`.

- [ ] **Step 1: Write the failing tests** in a new `crates/providers/src/credential.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AuthMethod;

    #[tokio::test]
    async fn api_key_resolves_the_first_set_env_var() {
        // A deliberately unique name that IS set for this test only.
        let var = "CODYPENDENT_TEST_PROVIDERS_KEY_7c1f";
        std::env::set_var(var, "sk-secret");
        let auth = AuthMethod::ApiKey {
            env: vec!["CODYPENDENT_TEST_PROVIDERS_UNSET_a1".to_string(), var.to_string()],
            header: "Authorization".to_string(),
            prefix: "Bearer ".to_string(),
        };
        let resolved = credential_for(&auth).resolve().await.expect("resolves");
        assert_eq!(
            resolved,
            ResolvedCredential::ApiKey {
                header: "Authorization".to_string(),
                prefix: "Bearer ".to_string(),
                value: "sk-secret".to_string(),
            }
        );
        std::env::remove_var(var);
    }

    #[tokio::test]
    async fn api_key_missing_env_errors_naming_the_variable() {
        let var = "CODYPENDENT_TEST_PROVIDERS_NEVER_SET_9f3c";
        assert!(std::env::var(var).is_err(), "precondition: {var} unset");
        let auth = AuthMethod::ApiKey {
            env: vec![var.to_string()],
            header: "Authorization".to_string(),
            prefix: "Bearer ".to_string(),
        };
        let err = credential_for(&auth).resolve().await.expect_err("must error");
        match &err {
            CredentialError::MissingEnv { var: v } => assert_eq!(v, var),
            other => panic!("expected MissingEnv, got {other:?}"),
        }
        assert!(err.to_string().contains(var), "message names the variable");
    }

    #[tokio::test]
    async fn none_and_acp_resolve_to_no_credential() {
        assert_eq!(credential_for(&AuthMethod::None).resolve().await.unwrap(), ResolvedCredential::None);
        let acp = AuthMethod::Acp { command: "gemini".into(), args: vec!["--acp".into()], env: Default::default() };
        assert_eq!(credential_for(&acp).resolve().await.unwrap(), ResolvedCredential::None);
    }

    #[tokio::test]
    async fn cloud_iam_and_oauth_are_not_wired() {
        let cloud = AuthMethod::CloudIam { variant: "aws_sigv4".into(), env: Default::default(), scopes: vec![] };
        assert!(matches!(credential_for(&cloud).resolve().await, Err(CredentialError::NotWired { .. })));
        let oauth = AuthMethod::OAuth { authorize_url: "x".into(), token_url: "y".into(), client_id: "z".into(), scopes: vec![], pkce: true };
        assert!(matches!(credential_for(&oauth).resolve().await, Err(CredentialError::NotWired { .. })));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p codypendent-providers api_key_resolves_the_first_set_env_var`
Expected: FAIL to compile — `credential` module does not exist.

- [ ] **Step 3: Implement the trait + impls** — the rest of `crates/providers/src/credential.rs`:

```rust
//! The credential-provider seam. Resolves auth material from the environment at
//! CALL TIME and never stores it (Chapter 11 secrets invariant). The trait is
//! `async` so the follow-up CloudIam/OAuth impls (token refresh, request signing)
//! slot in without changing this seam; the `ApiKey` impl resolves synchronously.

use async_trait::async_trait;

use crate::model::AuthMethod;

/// The concrete auth material a [`CredentialProvider`] resolved. Deliberately not
/// an HTTP `HeaderMap` — this leaf crate has no `http`/`reqwest` dep, and the
/// wired OpenAI-compatible path only needs the key string; a raw-HTTP adapter
/// (follow-up) can derive a header from `header`+`prefix`+`value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedCredential {
    /// No credential (local endpoints).
    None,
    /// A resolved API key: inject `value` under `header` with `prefix`.
    ApiKey {
        header: String,
        prefix: String,
        value: String,
    },
}

/// A failure resolving a credential.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CredentialError {
    /// None of the configured env-var NAMEs is set. Names the first, per the rule
    /// that secrets are identified (never guessed) in error output.
    #[error("environment variable `{var}` for the API key is not set")]
    MissingEnv { var: String },
    /// A credential method whose signing/refresh is a follow-up (CloudIam/OAuth).
    #[error("credential method `{method}` is not yet wired (follow-up PR)")]
    NotWired { method: &'static str },
}

/// Resolves the auth material to inject for one request, reading secrets from the
/// environment at call time.
#[async_trait]
pub trait CredentialProvider: Send + Sync {
    async fn resolve(&self) -> Result<ResolvedCredential, CredentialError>;
}

/// The wired API-key credential: the first `env` NAME that is set wins.
pub struct ApiKeyCredential {
    pub env: Vec<String>,
    pub header: String,
    pub prefix: String,
}

#[async_trait]
impl CredentialProvider for ApiKeyCredential {
    async fn resolve(&self) -> Result<ResolvedCredential, CredentialError> {
        for var in &self.env {
            if let Ok(value) = std::env::var(var) {
                return Ok(ResolvedCredential::ApiKey {
                    header: self.header.clone(),
                    prefix: self.prefix.clone(),
                    value,
                });
            }
        }
        match self.env.first() {
            Some(first) => Err(CredentialError::MissingEnv { var: first.clone() }),
            None => Ok(ResolvedCredential::None),
        }
    }
}

/// No-auth credential (local endpoints; ACP carries no HTTP credential).
pub struct NoneCredential;

#[async_trait]
impl CredentialProvider for NoneCredential {
    async fn resolve(&self) -> Result<ResolvedCredential, CredentialError> {
        Ok(ResolvedCredential::None)
    }
}

/// Trait-shaped stub: cloud-IAM signing/refresh is a follow-up.
pub struct CloudIamCredential;

#[async_trait]
impl CredentialProvider for CloudIamCredential {
    async fn resolve(&self) -> Result<ResolvedCredential, CredentialError> {
        Err(CredentialError::NotWired { method: "cloud-iam" })
    }
}

/// Trait-shaped stub: subscription OAuth is reserved and not wired (ToS-gated).
pub struct OAuthCredential;

#[async_trait]
impl CredentialProvider for OAuthCredential {
    async fn resolve(&self) -> Result<ResolvedCredential, CredentialError> {
        Err(CredentialError::NotWired { method: "oauth" })
    }
}

/// Build the credential provider for an auth method (a provider offers its methods
/// in preference order; the caller picks one — typically the first).
pub fn credential_for(method: &AuthMethod) -> Box<dyn CredentialProvider> {
    match method {
        AuthMethod::None | AuthMethod::Acp { .. } => Box::new(NoneCredential),
        AuthMethod::ApiKey { env, header, prefix } => Box::new(ApiKeyCredential {
            env: env.clone(),
            header: header.clone(),
            prefix: prefix.clone(),
        }),
        AuthMethod::CloudIam { .. } => Box::new(CloudIamCredential),
        AuthMethod::OAuth { .. } => Box::new(OAuthCredential),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p codypendent-providers credential::`
Expected: PASS (all four credential tests).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all -- --check && cargo clippy -p codypendent-providers --all-targets -- -D warnings
git add crates/providers/src/credential.rs crates/providers/src/lib.rs
git commit -m "feat(providers): async CredentialProvider trait + ApiKey impl (CloudIam/OAuth stubs)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 3: Built-in ~40-provider catalog, shadowable by user config

Embed the curated catalog and a loader that layers a user `providers.toml` over the built-ins (by id).

**Files:**
- Create: `crates/providers/builtin_catalog.toml`
- Create: `crates/providers/src/catalog.rs`
- Test: `crates/providers/src/catalog.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `Provider`, `ProvidersFile` (Task 1).
- Produces: `fn builtin_providers() -> Vec<Provider>`; `Catalog` with `builtin()`, `load_with_user_overrides(&Path) -> Result<Catalog, CatalogError>`, `get(&str) -> Option<&Provider>`, `providers() -> impl Iterator<Item = &Provider>`, `len()`, `is_empty()`; `CatalogError { Read, Parse }`.

- [ ] **Step 1: Create the embedded catalog** — `crates/providers/builtin_catalog.toml` (env vars and base URLs are from the research table; secrets are NAMEs only):

```toml
# Curated built-in provider catalog. Shadowable/extendable by the user's
# <data_dir>/providers.toml (same shape). Secrets are env-var NAMES only.

# --- Tier 1: frontier direct APIs (OpenAI-compatible unless noted) ---
[[provider]]
id = "openai"
name = "OpenAI"
protocol = "openai-chat"
base_url = "https://api.openai.com/v1"
[[provider.auth]]
kind = "api_key"
env = ["OPENAI_API_KEY"]

[[provider]]
id = "anthropic"
name = "Anthropic (Claude)"
protocol = "anthropic"
base_url = "https://api.anthropic.com"
extra_headers = { "anthropic-version" = "2023-06-01" }
[[provider.auth]]
kind = "api_key"
env = ["ANTHROPIC_API_KEY"]
header = "x-api-key"
prefix = ""

[[provider]]
id = "gemini"
name = "Google Gemini API"
protocol = "openai-chat"
base_url = "https://generativelanguage.googleapis.com/v1beta/openai/"
[[provider.auth]]
kind = "api_key"
env = ["GEMINI_API_KEY", "GOOGLE_API_KEY"]

[[provider]]
id = "xai"
name = "xAI (Grok)"
protocol = "openai-chat"
base_url = "https://api.x.ai/v1"
[[provider.auth]]
kind = "api_key"
env = ["XAI_API_KEY"]

[[provider]]
id = "deepseek"
name = "DeepSeek"
protocol = "openai-chat"
base_url = "https://api.deepseek.com"
[[provider.auth]]
kind = "api_key"
env = ["DEEPSEEK_API_KEY"]

[[provider]]
id = "mistral"
name = "Mistral"
protocol = "openai-chat"
base_url = "https://api.mistral.ai/v1"
[[provider.auth]]
kind = "api_key"
env = ["MISTRAL_API_KEY"]

[[provider]]
id = "moonshot"
name = "Moonshot AI (Kimi)"
protocol = "openai-chat"
base_url = "https://api.moonshot.ai/v1"
[[provider.auth]]
kind = "api_key"
env = ["MOONSHOT_API_KEY"]

[[provider]]
id = "zhipu"
name = "Zhipu / Z.ai (GLM)"
protocol = "openai-chat"
base_url = "https://api.z.ai/api/paas/v4"
[[provider.auth]]
kind = "api_key"
env = ["ZAI_API_KEY", "ZHIPUAI_API_KEY"]

[[provider]]
id = "minimax"
name = "MiniMax"
protocol = "openai-chat"
base_url = "https://api.minimax.io/v1"
[[provider.auth]]
kind = "api_key"
env = ["MINIMAX_API_KEY"]

[[provider]]
id = "qwen"
name = "Alibaba Qwen (DashScope)"
protocol = "openai-chat"
base_url = "https://dashscope-intl.aliyuncs.com/compatible-mode/v1"
[[provider.auth]]
kind = "api_key"
env = ["DASHSCOPE_API_KEY"]

# --- Tier 1: cloud enterprise (catalog entries; cloud-IAM signing is a follow-up) ---
[[provider]]
id = "azure-openai"
name = "Azure OpenAI (Foundry)"
protocol = "openai-chat"
# base_url is per-resource; the user sets it in their providers.toml (…/openai/v1/).
[[provider.auth]]
kind = "api_key"
env = ["AZURE_OPENAI_API_KEY"]
header = "api-key"
prefix = ""

[[provider]]
id = "amazon-bedrock"
name = "AWS Bedrock (mantle, bearer key)"
protocol = "openai-chat"
base_url = "https://bedrock-mantle.us-east-1.api.aws/v1"
[[provider.auth]]
kind = "api_key"
env = ["AWS_BEARER_TOKEN_BEDROCK"]

# --- Tier 2: inference aggregators / fast hosts (drop-in OpenAI-compatible) ---
[[provider]]
id = "openrouter"
name = "OpenRouter"
protocol = "openai-chat"
base_url = "https://openrouter.ai/api/v1"
[[provider.auth]]
kind = "api_key"
env = ["OPENROUTER_API_KEY"]

[[provider]]
id = "together"
name = "Together AI"
protocol = "openai-chat"
base_url = "https://api.together.ai/v1"
[[provider.auth]]
kind = "api_key"
env = ["TOGETHER_API_KEY"]

[[provider]]
id = "groq"
name = "Groq"
protocol = "openai-chat"
base_url = "https://api.groq.com/openai/v1"
[[provider.auth]]
kind = "api_key"
env = ["GROQ_API_KEY"]

[[provider]]
id = "fireworks"
name = "Fireworks AI"
protocol = "openai-chat"
base_url = "https://api.fireworks.ai/inference/v1"
[[provider.auth]]
kind = "api_key"
env = ["FIREWORKS_API_KEY"]

[[provider]]
id = "cerebras"
name = "Cerebras"
protocol = "openai-chat"
base_url = "https://api.cerebras.ai/v1"
[[provider.auth]]
kind = "api_key"
env = ["CEREBRAS_API_KEY"]

[[provider]]
id = "sambanova"
name = "SambaNova Cloud"
protocol = "openai-chat"
base_url = "https://api.sambanova.ai/v1"
[[provider.auth]]
kind = "api_key"
env = ["SAMBANOVA_API_KEY"]

[[provider]]
id = "deepinfra"
name = "DeepInfra"
protocol = "openai-chat"
base_url = "https://api.deepinfra.com/v1/openai"
[[provider.auth]]
kind = "api_key"
env = ["DEEPINFRA_TOKEN"]

[[provider]]
id = "novita"
name = "Novita AI"
protocol = "openai-chat"
base_url = "https://api.novita.ai/openai/v1"
[[provider.auth]]
kind = "api_key"
env = ["NOVITA_API_KEY"]

[[provider]]
id = "nebius"
name = "Nebius Token Factory"
protocol = "openai-chat"
base_url = "https://api.tokenfactory.nebius.com/v1/"
[[provider.auth]]
kind = "api_key"
env = ["NEBIUS_API_KEY"]

[[provider]]
id = "hyperbolic"
name = "Hyperbolic"
protocol = "openai-chat"
base_url = "https://api.hyperbolic.xyz/v1"
[[provider.auth]]
kind = "api_key"
env = ["HYPERBOLIC_API_KEY"]

[[provider]]
id = "baseten"
name = "Baseten Model APIs"
protocol = "openai-chat"
base_url = "https://inference.baseten.co/v1"
[[provider.auth]]
kind = "api_key"
env = ["BASETEN_API_KEY"]

[[provider]]
id = "lambda"
name = "Lambda Inference"
protocol = "openai-chat"
base_url = "https://api.lambda.ai/v1"
[[provider.auth]]
kind = "api_key"
env = ["LAMBDA_API_KEY"]

[[provider]]
id = "featherless"
name = "Featherless AI"
protocol = "openai-chat"
base_url = "https://api.featherless.ai/v1"
[[provider.auth]]
kind = "api_key"
env = ["FEATHERLESS_API_KEY"]

[[provider]]
id = "inference-net"
name = "Inference.net"
protocol = "openai-chat"
base_url = "https://api.inference.net/v1"
[[provider.auth]]
kind = "api_key"
env = ["INFERENCE_API_KEY"]

[[provider]]
id = "chutes"
name = "Chutes AI"
protocol = "openai-chat"
base_url = "https://llm.chutes.ai/v1"
[[provider.auth]]
kind = "api_key"
env = ["CHUTES_API_KEY"]

[[provider]]
id = "parasail"
name = "Parasail"
protocol = "openai-chat"
base_url = "https://api.parasail.io/v1"
[[provider.auth]]
kind = "api_key"
env = ["PARASAIL_API_KEY"]

[[provider]]
id = "venice"
name = "Venice AI"
protocol = "openai-chat"
base_url = "https://api.venice.ai/api/v1"
[[provider.auth]]
kind = "api_key"
env = ["VENICE_API_KEY"]

[[provider]]
id = "perplexity"
name = "Perplexity (Sonar)"
protocol = "openai-chat"
base_url = "https://api.perplexity.ai"
[[provider.auth]]
kind = "api_key"
env = ["PERPLEXITY_API_KEY"]

[[provider]]
id = "ai21"
name = "AI21 (Jamba)"
protocol = "openai-chat"
base_url = "https://api.ai21.com/studio/v1"
[[provider.auth]]
kind = "api_key"
env = ["AI21_API_KEY"]

[[provider]]
id = "cohere"
name = "Cohere (compat)"
protocol = "openai-chat"
base_url = "https://api.cohere.ai/compatibility/v1"
[[provider.auth]]
kind = "api_key"
env = ["CO_API_KEY", "COHERE_API_KEY"]

[[provider]]
id = "github-models"
name = "GitHub Models"
protocol = "openai-chat"
base_url = "https://models.github.ai/inference"
extra_headers = { "X-GitHub-Api-Version" = "2022-11-28" }
[[provider.auth]]
kind = "api_key"
env = ["GITHUB_TOKEN"]

[[provider]]
id = "opencode-zen"
name = "OpenCode Zen"
protocol = "openai-chat"
base_url = "https://opencode.ai/zen/v1"
[[provider.auth]]
kind = "api_key"
env = ["OPENCODE_API_KEY"]

# --- Tier 2: local (no auth) ---
[[provider]]
id = "ollama"
name = "Ollama (local)"
protocol = "openai-chat"
base_url = "http://localhost:11434/v1"
local = true
[[provider.auth]]
kind = "none"

[[provider]]
id = "lmstudio"
name = "LM Studio (local)"
protocol = "openai-chat"
base_url = "http://localhost:1234/v1"
local = true
[[provider.auth]]
kind = "none"

[[provider]]
id = "vllm"
name = "vLLM (local)"
protocol = "openai-chat"
base_url = "http://localhost:8000/v1"
local = true
[[provider.auth]]
kind = "none"

# --- ACP agents (delegate a turn; the agent owns its model) ---
[[provider]]
id = "gemini-cli"
name = "Gemini CLI (ACP)"
protocol = "acp"
[[provider.auth]]
kind = "acp"
command = "gemini"
args = ["--acp"]

[[provider]]
id = "claude-code"
name = "Claude Code (ACP)"
protocol = "acp"
[[provider.auth]]
kind = "acp"
command = "npx"
args = ["-y", "@agentclientprotocol/claude-agent-acp"]

[[provider]]
id = "codex"
name = "OpenAI Codex (ACP)"
protocol = "acp"
[[provider.auth]]
kind = "acp"
command = "npx"
args = ["-y", "@agentclientprotocol/codex-acp"]

[[provider]]
id = "opencode"
name = "OpenCode (ACP)"
protocol = "acp"
[[provider.auth]]
kind = "acp"
command = "opencode"
args = ["acp"]

[[provider]]
id = "cursor"
name = "Cursor CLI (ACP)"
protocol = "acp"
[[provider.auth]]
kind = "acp"
command = "agent"
args = ["acp"]
```

- [ ] **Step 2: Write the failing test** in a new `crates/providers/src/catalog.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Protocol;
    use std::io::Write;

    #[test]
    fn builtin_catalog_parses_and_has_known_providers() {
        let providers = builtin_providers();
        assert!(providers.len() >= 40, "expected ~40 providers, got {}", providers.len());
        let cat = Catalog::builtin();
        assert_eq!(cat.get("openai").map(|p| p.protocol), Some(Protocol::OpenAiChat));
        assert_eq!(cat.get("anthropic").map(|p| p.protocol), Some(Protocol::Anthropic));
        assert_eq!(cat.get("claude-code").map(|p| p.protocol), Some(Protocol::Acp));
        assert!(cat.get("ollama").is_some_and(|p| p.local));
        assert!(cat.get("groq").is_some());
    }

    #[test]
    fn a_user_providers_toml_shadows_a_builtin_by_id() {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        // Override the built-in `groq` base_url and add a brand-new provider.
        write!(
            file,
            r#"
[[provider]]
id = "groq"
name = "Groq (my proxy)"
protocol = "openai-chat"
base_url = "http://localhost:9000/v1"
[[provider.auth]]
kind = "none"

[[provider]]
id = "my-gateway"
name = "My Gateway"
protocol = "openai-chat"
base_url = "https://gw.example/v1"
[[provider.auth]]
kind = "api_key"
env = ["MY_GATEWAY_KEY"]
"#
        )
        .expect("write");

        let cat = Catalog::load_with_user_overrides(file.path()).expect("load");
        // Built-in groq is shadowed by the user entry.
        let groq = cat.get("groq").expect("groq present");
        assert_eq!(groq.base_url.as_deref(), Some("http://localhost:9000/v1"));
        // The user's new provider is added.
        assert!(cat.get("my-gateway").is_some());
        // Untouched built-ins remain.
        assert!(cat.get("openai").is_some());
    }
}
```

(`tempfile` is a dev-dependency; add `tempfile = { workspace = true }` to `crates/providers/Cargo.toml` `[dev-dependencies]` beside `tokio`.)

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p codypendent-providers builtin_catalog_parses_and_has_known_providers`
Expected: FAIL to compile — `catalog` module does not exist.

- [ ] **Step 4: Implement the loader** — the rest of `crates/providers/src/catalog.rs`:

```rust
//! The curated built-in provider catalog + a loader that layers a user
//! `providers.toml` over it (a user entry with the same `id` shadows a built-in).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::model::{Provider, ProvidersFile};

/// The curated catalog, embedded at build time.
const BUILTIN_CATALOG_TOML: &str = include_str!("../builtin_catalog.toml");

/// A failure loading a user provider catalog.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("failed to read provider catalog at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse provider catalog at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

/// Parse the embedded built-in catalog into its providers. Panics only on a
/// malformed embedded catalog — a build-time invariant pinned by a unit test.
#[must_use]
pub fn builtin_providers() -> Vec<Provider> {
    let file: ProvidersFile = toml::from_str(BUILTIN_CATALOG_TOML)
        .expect("the embedded built-in catalog is valid TOML");
    file.providers
}

/// The resolved provider catalog, keyed by id (BTreeMap → stable ordering for the
/// picker).
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    providers: BTreeMap<String, Provider>,
}

impl Catalog {
    /// The built-in catalog only.
    #[must_use]
    pub fn builtin() -> Self {
        Self::from_providers(builtin_providers())
    }

    /// Build from an explicit provider list (later ids overwrite earlier).
    pub fn from_providers(providers: impl IntoIterator<Item = Provider>) -> Self {
        let providers = providers.into_iter().map(|p| (p.id.clone(), p)).collect();
        Self { providers }
    }

    /// Built-ins, then the user's `providers.toml` layered on top (same id shadows;
    /// new ids extend). A missing user file is fine — the built-ins stand alone.
    pub fn load_with_user_overrides(path: &Path) -> Result<Self, CatalogError> {
        let mut providers: BTreeMap<String, Provider> = builtin_providers()
            .into_iter()
            .map(|p| (p.id.clone(), p))
            .collect();
        if path.exists() {
            let text = std::fs::read_to_string(path).map_err(|source| CatalogError::Read {
                path: path.to_path_buf(),
                source,
            })?;
            let file: ProvidersFile = toml::from_str(&text).map_err(|source| CatalogError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
            for p in file.providers {
                providers.insert(p.id.clone(), p);
            }
        }
        Ok(Self { providers })
    }

    /// Look up a provider by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Provider> {
        self.providers.get(id)
    }

    /// Iterate every provider, in id order.
    pub fn providers(&self) -> impl Iterator<Item = &Provider> {
        self.providers.values()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p codypendent-providers` (the whole crate — model, credential, catalog)
Expected: PASS.

- [ ] **Step 6: Full gate + commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
git add crates/providers/Cargo.toml crates/providers/builtin_catalog.toml crates/providers/src/catalog.rs crates/providers/src/lib.rs
git commit -m "feat(providers): curated ~40-provider built-in catalog + user override loader

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 4: Generalize `client_for` to build from `Protocol` + `CredentialProvider`

Rewrite `ModelRegistry::client_for` to map a `ModelConfig` onto a `Provider`, dispatch on `Protocol`, and resolve credentials through the async trait — returning an `Arc<dyn ChatClient>`. Today's `"openai-compatible"` config maps to `(OpenAiChat, [ApiKey])` and builds the exact same OpenAI client, so every existing `models.toml` runs unchanged. Other protocols return a clear `ProtocolNotWired` (Anthropic/Gemini native are follow-ups).

**Files:**
- Modify: `crates/runtime/Cargo.toml` (add `codypendent-providers`)
- Modify: `crates/runtime/src/models.rs` (async `client_for`; `ProtocolNotWired`; config→provider mapping; update tests)
- Modify: `crates/runtime/src/agent.rs` (`FrameworkModelDriver.client: Arc<dyn ChatClient>`; `new`/`from_registry` async)
- Modify: `crates/codypendentd/src/executor.rs:430`, `crates/codypendentd/src/workflow_exec.rs:355`, `crates/cli/src/commands.rs:1957` (the three `from_registry` call sites → `.await`)
- Test: `crates/runtime/src/models.rs` `#[cfg(test)]` (existing `client_for_*` tests → async)

**Interfaces:**
- Consumes: `codypendent_providers::{Protocol, AuthMethod, credential_for, ResolvedCredential, CredentialError}`; `agent_framework_core::client::ChatClient`; `agent_framework_openai::OpenAIChatCompletionClient`; existing `ModelConfig` (`models.rs:56`), `ModelsError` (`models.rs:109`).
- Produces: `async fn ModelRegistry::client_for(&self, id: &ModelId) -> Result<Arc<dyn ChatClient>>`; `FrameworkModelDriver::new(client: Arc<dyn ChatClient>, model_id)` and `async fn from_registry(...)`; new error `ModelsError::ProtocolNotWired { model, protocol }`.

- [ ] **Step 1: Wire the dependency**

In `crates/runtime/Cargo.toml` `[dependencies]`, after `codypendent-routing = { workspace = true }` (line 18), add:

```toml
codypendent-providers = { workspace = true }
```

- [ ] **Step 2: Update the existing `client_for` tests to the new async signature** (these are the failing tests that drive the change). In `crates/runtime/src/models.rs` tests, change the three `#[cfg(feature = "provider-openai")] #[test]` fns (`client_for_names_missing_env_var`, `client_for_allows_empty_api_key_env_for_local_endpoints`, `client_for_rejects_unsupported_provider`) to `#[tokio::test]` and `.await` the call. Replace their bodies' assertions that touch the concrete client:

```rust
#[cfg(feature = "provider-openai")]
#[tokio::test]
async fn client_for_names_missing_env_var() {
    let var_name = "CODYPENDENT_TEST_MODELS_RS_UNSET_KEY_9f3c7ab1";
    assert!(std::env::var(var_name).is_err(), "precondition: {var_name} unset");
    let id = model_id("hosted-default");
    let registry = ModelRegistry::new([ModelConfig {
        id: id.clone(),
        provider: "openai-compatible".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        model: "gpt-5.1-codex".to_string(),
        api_key_env: var_name.to_string(),
    }]);
    let err = registry.client_for(&id).await.err().expect("missing env must error");
    match &err {
        ModelsError::MissingApiKeyEnv { model, var } => {
            assert_eq!(model, &id);
            assert_eq!(var, var_name);
        }
        other => panic!("expected MissingApiKeyEnv, got {other:?}"),
    }
    assert!(err.to_string().contains(var_name));
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
    // Builds an OpenAI-compatible client with no key (Ok is enough — the concrete
    // model() accessor is no longer on the returned `Arc<dyn ChatClient>`).
    assert!(registry.client_for(&id).await.is_ok(), "empty api_key_env is not an error");
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
    let err = registry.client_for(&id).await.err().expect("unsupported provider must error");
    assert!(matches!(err, ModelsError::UnsupportedProvider { .. }));
}
```

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test -p codypendent-runtime --features provider-openai client_for_`
Expected: FAIL to compile — `client_for` is still sync and returns `OpenAIChatCompletionClient`.

- [ ] **Step 4: Add `ProtocolNotWired` and rewrite `client_for`**

In `crates/runtime/src/models.rs`, add these imports **gated with the same `#[cfg(feature = "provider-openai")]`** as the existing `use agent_framework_openai::OpenAIChatCompletionClient;` at line 36 — otherwise they are unused (and `config_to_protocol_auth` below is dead code) in a `--no-default-features` build, which the Linux CI lint rejects:

```rust
#[cfg(feature = "provider-openai")]
use std::sync::Arc;

#[cfg(feature = "provider-openai")]
use codypendent_providers::{credential_for, AuthMethod, CredentialError, Protocol, ResolvedCredential};
```

Add a variant to `ModelsError` (after `UnsupportedProvider`, ~line 136) — an enum variant is fine ungated (`ModelsError` is `#[non_exhaustive]`; an unconstructed variant does not warn):

```rust
    /// A model's provider maps to a wire protocol this build does not yet wire
    /// (Anthropic/Gemini native are follow-ups; only OpenAI-compatible is wired).
    #[error("model `{model}` uses protocol `{protocol}` which is not yet wired (only OpenAI-compatible is)")]
    ProtocolNotWired { model: ModelId, protocol: String },
```

Add a private mapping helper (near `client_for`, **gated `#[cfg(feature = "provider-openai")]`** because `client_for` is its only caller), turning today's `ModelConfig` into the new `(Protocol, AuthMethod)` shape — this is the backward-compatible bridge:

```rust
/// Map a legacy [`ModelConfig`] onto the new provider abstraction: today's only
/// supported `provider = "openai-compatible"` becomes `(OpenAiChat, ApiKey|None)`.
/// An empty `api_key_env` means no key (local endpoints) → `AuthMethod::None`.
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
```

Replace the `#[cfg(feature = "provider-openai")] impl ModelRegistry { pub fn client_for ... }` block (`models.rs:205-240`) with:

```rust
#[cfg(feature = "provider-openai")]
impl ModelRegistry {
    /// Build a framework chat client for `id`, dispatching on the model's wire
    /// protocol and resolving credentials through the async [`CredentialProvider`]
    /// seam. Reads the API key from its env var right here, at call time — moved
    /// straight into the client, never stored, logged, or retained (Chapter 11).
    ///
    /// Today only [`Protocol::OpenAiChat`] is wired; it returns the same
    /// `OpenAIChatCompletionClient` as before, now behind `Arc<dyn ChatClient>`.
    pub async fn client_for(&self, id: &ModelId) -> Result<Arc<dyn agent_framework_core::client::ChatClient>> {
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
                    Err(CredentialError::NotWired { method }) => {
                        return Err(ModelsError::ProtocolNotWired {
                            model: id.clone(),
                            protocol: method.to_string(),
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
```

- [ ] **Step 5: Change `FrameworkModelDriver` to hold `Arc<dyn ChatClient>`**

In `crates/runtime/src/agent.rs`, change the struct + constructors (`agent.rs:2187-2208`):

```rust
#[cfg(feature = "provider-openai")]
pub struct FrameworkModelDriver {
    client: std::sync::Arc<dyn agent_framework_core::client::ChatClient>,
    model_id: ModelId,
}

#[cfg(feature = "provider-openai")]
impl FrameworkModelDriver {
    /// Wrap a constructed client and record the model id it serves.
    pub fn new(
        client: std::sync::Arc<dyn agent_framework_core::client::ChatClient>,
        model_id: ModelId,
    ) -> Self {
        Self { client, model_id }
    }

    /// Build a driver from the registry by resolving `model_id` to a client.
    pub async fn from_registry(models: &ModelRegistry, model_id: ModelId) -> anyhow::Result<Self> {
        let client = models
            .client_for(&model_id)
            .await
            .map_err(|e| anyhow::anyhow!("could not build client for {model_id}: {e}"))?;
        Ok(Self::new(client, model_id))
    }
}
```

`next_step` is unchanged: it does `use agent_framework_core::client::ChatClient;` then `self.client.get_streaming_response(...)`, which resolves via the crate's blanket `impl<T: ChatClient + ?Sized> ChatClient for Arc<T>` (`agent-framework-core-0.1.1/src/client.rs:56`). No other edit in this impl.

- [ ] **Step 6: `.await` the three `from_registry` call sites**

All three are already inside `async fn`s; only `.await` is added.

`crates/codypendentd/src/executor.rs:430` and `crates/codypendentd/src/workflow_exec.rs:355` each become:

```rust
        let driver = FrameworkModelDriver::from_registry(&registry, model_id)
            .await
            .map_err(|e| format!("could not build model client: {e}"))?;
```

`crates/cli/src/commands.rs:1957` (the `models bench` command; note it uses anyhow `.with_context`) becomes:

```rust
    let driver = FrameworkModelDriver::from_registry(&registry, config.id.clone())
        .await
        .with_context(|| format!("building a model client for `{id}`"))?;
```

- [ ] **Step 7: Run the tests**

Run: `cargo test -p codypendent-runtime --features provider-openai client_for_`
Expected: PASS.
Run: `cargo build --workspace --all-features` (the three `from_registry` call sites in `codypendentd` + `cli` compile with the added `.await`).
Expected: PASS.

- [ ] **Step 8: Full gate + commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
git add crates/runtime/Cargo.toml crates/runtime/src/models.rs crates/runtime/src/agent.rs crates/codypendentd/src/executor.rs crates/codypendentd/src/workflow_exec.rs crates/cli/src/commands.rs
git commit -m "feat(runtime): build model clients from Protocol + CredentialProvider

Generalizes ModelRegistry::client_for to dispatch on the provider Protocol and
resolve credentials through the async CredentialProvider seam, returning
Arc<dyn ChatClient>. Legacy models.toml (\"openai-compatible\") maps to
(OpenAiChat, ApiKey) and builds the identical OpenAI client — fully backward
compatible. Non-OpenAI protocols return ProtocolNotWired (follow-ups).

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 5: Add the `agent-client-protocol` dependency + `cargo deny` gate

Introduce the only new external dependency and prove it passes the supply-chain gate, as its own reviewable step.

**Files:**
- Modify: `Cargo.toml` (root — `[workspace.dependencies]`)
- Modify: `crates/integrations/Cargo.toml`
- Modify (only if required): `deny.toml`
- Create: `crates/integrations/src/acp_client.rs` (empty stub module so the crate compiles with the dep)
- Modify: `crates/integrations/src/lib.rs` (`pub mod acp_client;`)

**Interfaces:**
- Produces: the `agent_client_protocol` crate available to `crates/integrations`.

- [ ] **Step 1: Add the dependency**

In root `Cargo.toml` `[workspace.dependencies]`, add (near the other external deps, e.g. after the `reqwest`/`hmac` block):

```toml
# ACP (Agent Client Protocol) — Zed's reference Rust impl (agentclientprotocol/rust-sdk).
# Used by crates/integrations' ACP *client* (acp_client.rs) to delegate a run to
# an external agent subprocess over JSON-RPC 2.0 stdio. The only new external dep.
agent-client-protocol = "2"
```

In `crates/integrations/Cargo.toml` `[dependencies]`, add:

```toml
agent-client-protocol = { workspace = true }
futures = { workspace = true }
```

(`futures` drives the ACP notification stream; it is already a workspace dep.)

Create `crates/integrations/src/acp_client.rs` with a module doc only (filled in Tasks 6-7):

```rust
//! ACP (Agent Client Protocol) *client* — the inverse of `acp.rs`.
//!
//! `acp.rs` is the SERVER role (Codypendent serves ACP to Zed). This module is
//! the CLIENT/host role: Codypendent spawns an external ACP agent
//! (`gemini --acp`, `npx @agentclientprotocol/claude-agent-acp`, ...), does the
//! initialize/session handshake, delegates a run's objective as an ACP prompt,
//! and maps the agent's streamed `session/update`s onto Codypendent's existing
//! `EventBody` model. The agent owns its model; we send no model id.
```

Add to `crates/integrations/src/lib.rs` (after `pub mod acp;`, line 20):

```rust
pub mod acp_client;
```

- [ ] **Step 2: Verify the build resolves the dependency**

Run: `cargo build -p codypendent-integrations`
Expected: PASS (the crate compiles with the new dep; `agent-client-protocol` and its transitive deps are fetched).

- [ ] **Step 3: Run the supply-chain gate**

Run: `cargo deny check licenses bans sources`
Expected: PASS. `agent-client-protocol` is Apache-2.0 (in `deny.toml`'s allow-list). **If** the gate reports a new transitive crate whose licence is not allowed, add that exact SPDX id to `deny.toml`'s `[licenses] allow` list with a one-line justification (mirroring the existing entries), or the crate to `[bans] skip` if it is a duplicate major version — then re-run. Do not disable a check.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings
git add Cargo.toml Cargo.lock crates/integrations/Cargo.toml crates/integrations/src/lib.rs crates/integrations/src/acp_client.rs deny.toml
git commit -m "build(integrations): add agent-client-protocol dep (cargo deny clean)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

(Stage `deny.toml` only if Step 3 required an edit.)

---

## Task 6: Pure ACP `session/update` → `EventBody` mapping

The load-bearing, fully-deterministic core of the ACP client: translate an ACP `session/update` body onto Codypendent's existing events. This mirrors the *inverse* mapping the server-side bridge already does (`crates/cli/src/acp.rs:162-192`).

**Files:**
- Modify: `crates/integrations/src/acp_client.rs`
- Test: `crates/integrations/src/acp_client.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `codypendent_protocol::{EventBody, RunId, ToolOutcome}` (re-exported from the protocol crate root — `events::EventBody`, `ids::RunId`, `run::ToolOutcome`); `serde_json::Value`.
- Produces: `pub fn session_update_to_events(update: &Value, run_id: RunId) -> Vec<EventBody>`.

**VERIFY-FIRST (field names):** before writing the mapping body, confirm the ACP `session/update` JSON field names against `agent-client-protocol`'s schema (`docs.rs/agent-client-protocol`, type `SessionUpdate`/`SessionNotification`; research §d). The mapping keys on the `sessionUpdate` discriminator (`agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`) and reads text from `content.text`. If the real field paths differ, adjust the extraction *only* (the kind→`EventBody` contract below, and the test, are the invariant). The test uses these same field names, so it stays self-consistent.

- [ ] **Step 1: Write the failing test** (append to `acp_client.rs`):

```rust
#[cfg(test)]
mod mapping_tests {
    use super::*;
    use codypendent_protocol::{EventBody, RunId, ToolOutcome};
    use serde_json::json;

    fn rid() -> RunId {
        RunId::new()
    }

    #[test]
    fn agent_message_chunk_maps_to_a_model_stream_delta() {
        let run_id = rid();
        let update = json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "hello" }
        });
        let events = session_update_to_events(&update, run_id);
        assert_eq!(events, vec![EventBody::ModelStreamDelta { run_id, text: "hello".to_string() }]);
    }

    #[test]
    fn agent_thought_chunk_also_streams_as_text() {
        let run_id = rid();
        let update = json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": "thinking" } });
        let events = session_update_to_events(&update, run_id);
        assert_eq!(events, vec![EventBody::ModelStreamDelta { run_id, text: "thinking".to_string() }]);
    }

    #[test]
    fn tool_call_maps_to_tool_started() {
        let run_id = rid();
        let update = json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "read_file", "status": "pending" });
        let events = session_update_to_events(&update, run_id);
        assert_eq!(
            events,
            vec![EventBody::ToolStarted { run_id, tool: "read_file".to_string(), args_digest: String::new() }]
        );
    }

    #[test]
    fn completed_tool_call_update_maps_to_tool_completed_succeeded() {
        let run_id = rid();
        let update = json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "title": "read_file", "status": "completed" });
        let events = session_update_to_events(&update, run_id);
        assert_eq!(
            events,
            vec![EventBody::ToolCompleted { run_id, tool: "read_file".to_string(), outcome: ToolOutcome::Succeeded, artifact: None }]
        );
    }

    #[test]
    fn failed_tool_call_update_maps_to_tool_completed_failed() {
        let run_id = rid();
        let update = json!({ "sessionUpdate": "tool_call_update", "title": "shell", "status": "failed" });
        let events = session_update_to_events(&update, run_id);
        assert!(matches!(events.as_slice(), [EventBody::ToolCompleted { outcome: ToolOutcome::Failed { .. }, .. }]));
    }

    #[test]
    fn an_empty_chunk_and_an_in_progress_update_produce_no_events() {
        let run_id = rid();
        assert!(session_update_to_events(&json!({ "sessionUpdate": "agent_message_chunk", "content": { "text": "" } }), run_id).is_empty());
        assert!(session_update_to_events(&json!({ "sessionUpdate": "tool_call_update", "title": "x", "status": "in_progress" }), run_id).is_empty());
        assert!(session_update_to_events(&json!({ "sessionUpdate": "plan" }), run_id).is_empty());
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p codypendent-integrations agent_message_chunk_maps_to_a_model_stream_delta`
Expected: FAIL to compile — `session_update_to_events` not defined.

- [ ] **Step 3: Implement the mapping** (add to `acp_client.rs`, above the `#[cfg(test)]`):

```rust
use codypendent_protocol::{EventBody, RunId, ToolOutcome};
use serde_json::Value;

/// Map one ACP `session/update` body onto zero or more Codypendent events for the
/// run it belongs to. Unknown/in-progress updates map to nothing (additive: the
/// TUI renders an ACP-backed turn from the same events as a native one).
///
/// The inverse of the server-side bridge in `crates/cli/src/acp.rs`.
#[must_use]
pub fn session_update_to_events(update: &Value, run_id: RunId) -> Vec<EventBody> {
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "agent_message_chunk" | "agent_thought_chunk" => {
            let text = update
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if text.is_empty() {
                Vec::new()
            } else {
                vec![EventBody::ModelStreamDelta {
                    run_id,
                    text: text.to_string(),
                }]
            }
        }
        "tool_call" => {
            let tool = tool_label(update);
            vec![EventBody::ToolStarted {
                run_id,
                tool,
                // The agent runs the tool; we did not build the args, so there is
                // no digest to record (never fabricate one).
                args_digest: String::new(),
            }]
        }
        "tool_call_update" => {
            let status = update.get("status").and_then(Value::as_str).unwrap_or_default();
            let outcome = match status {
                "completed" => ToolOutcome::Succeeded,
                "failed" => ToolOutcome::Failed {
                    message: "acp tool call failed".to_string(),
                },
                _ => return Vec::new(), // pending / in_progress: not terminal yet.
            };
            vec![EventBody::ToolCompleted {
                run_id,
                tool: tool_label(update),
                outcome,
                artifact: None,
            }]
        }
        _ => Vec::new(),
    }
}

/// A human tool label from an ACP tool update (`title`, else `kind`, else `tool`).
fn tool_label(update: &Value) -> String {
    update
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| update.get("kind").and_then(Value::as_str))
        .unwrap_or("tool")
        .to_string()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p codypendent-integrations mapping_tests`
Expected: PASS (all six).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/integrations/src/acp_client.rs
git commit -m "feat(integrations): map ACP session/update onto Codypendent events

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 7: ACP client — spawn, handshake, delegate a prompt, map updates (mock-agent test)

Build the client that connects to an external ACP agent, runs `initialize` → `session/new` → `session/prompt`, streams the agent's `session/update`s (via Task 6's mapping) to an event sink, and answers `session/request_permission`. Test it end-to-end against a **mock stdio JSON-RPC agent** over an in-memory duplex (mirroring the harness in `crates/integrations/src/acp.rs`'s tests).

**Files:**
- Modify: `crates/integrations/src/acp_client.rs`
- Test: `crates/integrations/src/acp_client.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `agent_client_protocol` (connection + client trait); Task 6's `session_update_to_events`; `crates/integrations/src/acp.rs`'s `PermissionOption` (re-use: `use crate::acp::PermissionOption;`); `codypendent_protocol::{EventBody, RunId}`.
- Produces: `pub enum AcpClientError`; `#[async_trait] pub trait AcpEventSink { async fn on_event(&mut self, event: EventBody); async fn on_permission(&mut self, tool_call: Value, options: Vec<PermissionOption>) -> Option<String>; }`; `pub struct AcpClient`; `AcpClient::connect<R,W>(reader: R, writer: W, cwd: &str) -> Result<AcpClient, AcpClientError>`; `AcpClient::spawn(command: &str, args: &[String], cwd: &str) -> Result<AcpClient, AcpClientError>`; `AcpClient::prompt(&mut self, objective: &str, run_id: RunId, sink: &mut dyn AcpEventSink) -> Result<AcpStopReason, AcpClientError>`; `pub enum AcpStopReason { EndTurn, Cancelled, Refusal }`.

**VERIFY-FIRST (crate API):** before writing `connect`/`prompt`, read `agent-client-protocol`'s docs and its `examples/yolo_one_shot_client.rs` (research §d) to confirm the exact client-connection API: the `ClientSideConnection` constructor (it takes a client-callback handler + an outgoing `AsyncWrite` + an incoming `AsyncRead` + a spawn fn), the `Client` trait you implement for the agent's callbacks (`request_permission`, `fs/*`, `terminal/*`), and the `Agent` methods you call (`initialize`, `new_session`/`session_new`, `prompt`/`session_prompt`) plus the `SessionNotification` type carrying `session/update`. Implement the callbacks minimally: `request_permission` delegates to `sink.on_permission`; `fs/*` and `terminal/*` return an unsupported/empty result (this PR does not grant the agent host fs/terminal — a follow-up). Wire the connection's outgoing/incoming to the `reader`/`writer` passed to `connect`, so the mock test can drive it over a duplex. Keep `session_update_to_events` (Task 6) as the single translation point for streamed updates.

- [ ] **Step 1: Write the failing mock-agent test** (append to `acp_client.rs`). It scripts a minimal ACP **agent** peer over `tokio::io::duplex`, connects the client, delegates a prompt, and asserts the mapped events + stop reason:

```rust
#[cfg(test)]
mod client_tests {
    use super::*;
    use codypendent_protocol::{EventBody, RunId};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // A sink that records mapped events and auto-approves any permission request.
    struct RecordingSink {
        events: Arc<Mutex<Vec<EventBody>>>,
    }
    #[async_trait::async_trait]
    impl AcpEventSink for RecordingSink {
        async fn on_event(&mut self, event: EventBody) {
            self.events.lock().unwrap().push(event);
        }
        async fn on_permission(&mut self, _tool_call: Value, options: Vec<crate::acp::PermissionOption>) -> Option<String> {
            options.first().map(|o| o.option_id.clone())
        }
    }

    // A scripted ACP AGENT peer: answers initialize/session.new, streams one text
    // chunk + one tool_call on prompt, then returns stopReason end_turn. Reads
    // newline-delimited JSON-RPC on `agent_in`, writes it on `agent_out`.
    async fn scripted_agent<R, W>(mut agent_in: BufReader<R>, mut agent_out: W)
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut line = String::new();
        loop {
            line.clear();
            let n = agent_in.read_line(&mut line).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            let msg: Value = match serde_json::from_str(line.trim()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
            let id = msg.get("id").cloned();
            let reply = |out: &mut W, body: Value| async move {
                let mut s = serde_json::to_string(&body).unwrap();
                s.push('\n');
                out.write_all(s.as_bytes()).await.unwrap();
                out.flush().await.unwrap();
            };
            match method {
                "initialize" => {
                    reply(&mut agent_out, json!({ "jsonrpc": "2.0", "id": id, "result": { "protocolVersion": 1, "agentCapabilities": {} } })).await;
                }
                "session/new" | "session/load" => {
                    reply(&mut agent_out, json!({ "jsonrpc": "2.0", "id": id, "result": { "sessionId": "s-1" } })).await;
                }
                "session/prompt" => {
                    // Stream a text chunk, then a tool_call, then resolve the turn.
                    reply(&mut agent_out, json!({ "jsonrpc": "2.0", "method": "session/update",
                        "params": { "sessionId": "s-1", "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "hi from agent" } } } })).await;
                    reply(&mut agent_out, json!({ "jsonrpc": "2.0", "method": "session/update",
                        "params": { "sessionId": "s-1", "update": { "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "read_file", "status": "pending" } } })).await;
                    reply(&mut agent_out, json!({ "jsonrpc": "2.0", "id": id, "result": { "stopReason": "end_turn" } })).await;
                }
                _ => {
                    if id.is_some() {
                        reply(&mut agent_out, json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "method not found" } })).await;
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn client_delegates_a_prompt_and_maps_streamed_updates() {
        // client <-> agent over two duplex pipes (client stdin/stdout ↔ agent stdout/stdin).
        let (client_reads, agent_writes) = tokio::io::duplex(8192); // agent -> client
        let (agent_reads, client_writes) = tokio::io::duplex(8192); // client -> agent
        tokio::spawn(scripted_agent(BufReader::new(agent_reads), agent_writes));

        let mut client = AcpClient::connect(client_reads, client_writes, "/tmp/repo")
            .await
            .expect("handshake completes");

        let events = Arc::new(Mutex::new(Vec::new()));
        let mut sink = RecordingSink { events: events.clone() };
        let run_id = RunId::new();

        let stop = client.prompt("do the thing", run_id, &mut sink).await.expect("prompt resolves");
        assert!(matches!(stop, AcpStopReason::EndTurn));

        let events = events.lock().unwrap().clone();
        assert!(events.contains(&EventBody::ModelStreamDelta { run_id, text: "hi from agent".to_string() }));
        assert!(events.iter().any(|e| matches!(e, EventBody::ToolStarted { tool, .. } if tool == "read_file")));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p codypendent-integrations client_delegates_a_prompt_and_maps_streamed_updates`
Expected: FAIL to compile — `AcpClient`/`AcpEventSink`/`AcpStopReason` not defined.

- [ ] **Step 3: Implement the client surface + transport** (add to `acp_client.rs`). The surface types + `spawn` below are complete; `connect`/`prompt` follow as a precise behaviour contract implemented against the `agent_client_protocol` API confirmed in VERIFY-FIRST. This is the contract every step above relies on.

```rust
use async_trait::async_trait;

use crate::acp::PermissionOption;

/// Why an ACP prompt turn ended (mirrors `crate::acp::StopReason`, kept local so
/// the client role owns its own type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpStopReason {
    EndTurn,
    Cancelled,
    Refusal,
}

/// A failure in the ACP client.
#[derive(Debug, thiserror::Error)]
pub enum AcpClientError {
    #[error("acp client I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("acp handshake failed: {0}")]
    Handshake(String),
    #[error("acp prompt failed: {0}")]
    Prompt(String),
}

/// Receives the events an ACP turn produces and answers the agent's permission
/// requests. The daemon implements this to fan mapped events into a run's ledger
/// and to route a permission through the existing approval broker (follow-up
/// wiring); tests implement it to record/auto-answer.
#[async_trait]
pub trait AcpEventSink: Send {
    /// A Codypendent event mapped from a streamed `session/update`.
    async fn on_event(&mut self, event: EventBody);
    /// Answer an ACP `session/request_permission`: return the chosen `optionId`,
    /// or `None` to cancel.
    async fn on_permission(&mut self, tool_call: Value, options: Vec<PermissionOption>) -> Option<String>;
}

/// A connected ACP agent session. Holds the `agent_client_protocol` connection
/// handle, the negotiated session id, and (for the spawn path) the child process
/// so dropping the client tears the agent down.
pub struct AcpClient {
    // Confirmed in VERIFY-FIRST: e.g. `conn: agent_client_protocol::ClientSideConnection`.
    conn: AcpConnection,
    session_id: String,
    #[allow(dead_code)]
    child: Option<tokio::process::Child>,
}

impl AcpClient {
    /// Spawn `command args` as a child and connect over its stdio (production path).
    /// Complete — mirrors the spawn+piped-stdio pattern in `crates/sandbox/src/executor.rs:568`.
    pub async fn spawn(command: &str, args: &[String], cwd: &str) -> Result<AcpClient, AcpClientError> {
        use tokio::process::Command;
        let mut child = Command::new(command)
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit()) // agent logs → our stderr
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AcpClientError::Handshake("no child stdout".into()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AcpClientError::Handshake("no child stdin".into()))?;
        let mut client = AcpClient::connect(stdout, stdin, cwd).await?;
        client.child = Some(child);
        Ok(client)
    }
}
```

**`connect` and `prompt` — implement against the `agent-client-protocol` API confirmed in VERIFY-FIRST.** These two methods are the only crate-API-dependent code, so they are specified as an exact behaviour contract rather than guessed method signatures (do not fabricate crate symbols; use the ones VERIFY-FIRST confirms). The mock-agent test in Step 1 is the acceptance gate.

`pub async fn connect<R, W>(reader: R, writer: W, cwd: &str) -> Result<AcpClient, AcpClientError>` where `R: AsyncRead + Unpin + Send + 'static`, `W: AsyncWrite + Unpin + Send + 'static`:
1. Construct the crate's client connection binding `writer` as the outgoing byte sink and `reader` as the incoming byte source (plus a `tokio::spawn` fn), with a `Client` callback handler whose `request_permission` forwards to a shared sink slot the running prompt installs (a `tokio::sync::Mutex<Option<*mut dyn AcpEventSink>>`-style handoff, or an `mpsc` request/response pair), and whose `fs/*` and `terminal/*` callbacks return an unsupported/empty result (this PR grants the agent no host fs/terminal — a follow-up).
2. `await` `initialize` with `{ protocolVersion: 1, clientCapabilities: {} }`; on transport/negotiation failure return `AcpClientError::Handshake(_)`.
3. `await` `new_session` with `{ cwd, mcpServers: [] }`, capturing the returned `sessionId`.
4. Return `AcpClient { conn, session_id, child: None }`.

`pub async fn prompt(&mut self, objective: &str, run_id: RunId, sink: &mut dyn AcpEventSink) -> Result<AcpStopReason, AcpClientError>`:
1. Install `sink` into the shared slot the `Client` handler reads for permission requests (so `on_permission` answers reach it).
2. Send `prompt` with `{ sessionId: self.session_id, prompt: [ { "type": "text", "text": objective } ] }`. No model id is sent — the agent owns its model.
3. As each incoming `session/update` `SessionNotification` arrives, extract its `update` JSON and run **exactly** `for ev in session_update_to_events(&update, run_id) { sink.on_event(ev).await; }` — the single translation point (Task 6); never re-map inline.
4. When the `Client` handler's `request_permission` fires, call `sink.on_permission(tool_call, options).await`; reply `{ outcome: { outcome: "selected", optionId } }` for `Some(id)` or `{ outcome: { outcome: "cancelled" } }` for `None`.
5. On the prompt's resolution, map the `stopReason` string: `"end_turn" | "max_tokens" | "max_turn_requests" => AcpStopReason::EndTurn`, `"cancelled" => Cancelled`, `"refusal" => Refusal`. Map any error to `AcpClientError::Prompt(_)`.

`AcpConnection` is the type alias for whatever the crate returns from its client-connection constructor (confirm in VERIFY-FIRST; e.g. `type AcpConnection = agent_client_protocol::ClientSideConnection;`).

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p codypendent-integrations client_delegates_a_prompt_and_maps_streamed_updates`
Expected: PASS — the client completes the handshake, delegates the prompt, and the sink records a `ModelStreamDelta("hi from agent")` and a `ToolStarted("read_file")`, with `AcpStopReason::EndTurn`.

- [ ] **Step 5: Add a permission-path test + confirm it passes**

Extend `scripted_agent` with a branch that, on `session/prompt`, first sends a `session/request_permission` request (`{"jsonrpc":"2.0","id":99,"method":"session/request_permission","params":{"sessionId":"s-1","toolCall":{...},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]}}`), reads the client's response line, and only then streams the update + resolves. Add:

```rust
    #[tokio::test]
    async fn client_answers_a_permission_request_with_the_sinks_choice() {
        // build duplex pipes + spawn the permission-scripted agent, connect, prompt;
        // assert the agent received {outcome:{outcome:"selected",optionId:"allow"}}
        // (the scripted agent asserts this and only then resolves end_turn).
    }
```

(Have the scripted agent capture the permission response and assert `params?`→`result.outcome.outcome == "selected"` and `optionId == "allow"`; the `RecordingSink::on_permission` returns the first option's id.)

Run: `cargo test -p codypendent-integrations client_`
Expected: PASS (both client tests).

- [ ] **Step 6: Full gate + commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
git add crates/integrations/src/acp_client.rs
git commit -m "feat(integrations): ACP client — handshake, delegate a prompt, map updates

Connects to an external ACP agent subprocess, runs initialize/session/new/
session/prompt, streams the agent's session/updates onto Codypendent EventBody
via session_update_to_events, and answers session/request_permission. Verified
against a mock stdio JSON-RPC agent.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 8: TUI `/provider` picker over the catalog

Add a `/provider` command + overlay listing the catalog (mirroring the merged `/model` picker exactly), seeded by the CLI from `codypendent_providers::Catalog`. Browses/stages a provider for the next run; wiring a staged provider into a live run is a follow-up (needs the auth state machine).

**Files:**
- Modify: `crates/tui/src/palette.rs` (`PaletteCommand::Provider` + `COMMANDS` row)
- Modify: `crates/tui/src/state.rs` (`ProviderCard`, `Overlay::ProviderPicker`, `AppState.providers/selected_provider/pending_provider`, `filter_providers`)
- Modify: `crates/tui/src/reduce.rs` (open/nav/filter/stage arms mirroring the model picker)
- Modify: `crates/cli/src/tui.rs` (seed `state.providers` from the catalog)
- Test: `crates/tui/src/palette.rs` + `crates/tui/src/reduce.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `codypendent_providers::{Catalog, AuthMethod}` (in the CLI seed); the model-picker pattern — `PaletteCommand::Model` (`palette.rs:37`), `Overlay::ModelPicker { query, selected }` (`state.rs:146`), `filter_models` (`state.rs:674`), `run_palette_command` Model arm (`reduce.rs:1128`), nav (`reduce.rs:655`), edit (`reduce.rs:955`), submit (`reduce.rs:1068`).
- Produces: `PaletteCommand::Provider`; `Overlay::ProviderPicker { query: String, selected: usize }`; `ProviderCard { id, name, protocol, auth, local }`; `AppState.providers: Vec<ProviderCard>`, `AppState.selected_provider: usize`, `AppState.pending_provider: Option<String>`; `fn filter_providers(&[ProviderCard], &str) -> Vec<usize>`.

- [ ] **Step 1: Write the failing palette test** (in `crates/tui/src/palette.rs` tests):

```rust
#[test]
fn filters_to_the_provider_picker_command() {
    let provider = filtered("provider");
    assert_eq!(provider.len(), 1);
    assert_eq!(provider[0].command, PaletteCommand::Provider);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p codypendent-tui filters_to_the_provider_picker_command`
Expected: FAIL — `PaletteCommand::Provider` not defined.

- [ ] **Step 3: Add the palette command + row**

In `crates/tui/src/palette.rs`, add to `enum PaletteCommand` (near `Model`, line 37):

```rust
    /// Open the provider catalog picker.
    Provider,
```

Add a `COMMANDS` row (after the `Model` row, `palette.rs:120`):

```rust
    PaletteEntry {
        command: PaletteCommand::Provider,
        title: "Provider catalog",
        description: "browse the built-in provider catalog and stage one",
        key: "—",
    },
```

- [ ] **Step 4: Add state + a failing reducer test**

In `crates/tui/src/state.rs`, add (near `ModelCard`, `state.rs:647`):

```rust
/// One provider-catalog row for the `/provider` picker projection (the TUI does no
/// I/O; the CLI seeds this from `codypendent_providers::Catalog`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCard {
    pub id: String,
    pub name: String,
    /// Wire protocol label, e.g. "openai-chat" | "anthropic" | "acp".
    pub protocol: String,
    /// Auth label, e.g. "api-key: GROQ_API_KEY" | "none" | "acp: npx …".
    pub auth: String,
    pub local: bool,
}
```

Add an `Overlay` variant (beside `ModelPicker`, `state.rs:146`):

```rust
    /// The provider-catalog picker.
    ProviderPicker { query: String, selected: usize },
```

Add the `Overlay::ProviderPicker { .. }` mapping to `InputMode::Palette` beside the existing `Overlay::Palette | Overlay::ModelPicker` arm (`state.rs:881`). Add fields to `AppState` (beside `models`/`selected_model`/`pending_model`, `state.rs:773-787`):

```rust
    /// The provider catalog for the `/provider` picker (seeded once at attach).
    pub providers: Vec<ProviderCard>,
    /// Focused row in the provider picker.
    pub selected_provider: usize,
    /// The provider staged for the next run (browse/stage only this PR).
    pub pending_provider: Option<String>,
```

(Initialize `providers: Vec::new(), selected_provider: 0, pending_provider: None` in `AppState::new`/`Default`.)

Add `filter_providers` (beside `filter_models`, `state.rs:674`):

```rust
/// Indices of provider cards matching `query` (substring over id, name, protocol).
#[must_use]
pub fn filter_providers(providers: &[ProviderCard], query: &str) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    providers
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            needle.is_empty()
                || c.id.to_lowercase().contains(&needle)
                || c.name.to_lowercase().contains(&needle)
                || c.protocol.to_lowercase().contains(&needle)
        })
        .map(|(i, _)| i)
        .collect()
}
```

Failing reducer test (in `crates/tui/src/reduce.rs` tests):

```rust
#[test]
fn provider_picker_opens_filters_and_stages() {
    let mut s = AppState::new();
    s.providers = vec![
        ProviderCard { id: "groq".into(), name: "Groq".into(), protocol: "openai-chat".into(), auth: "api-key: GROQ_API_KEY".into(), local: false },
        ProviderCard { id: "ollama".into(), name: "Ollama (local)".into(), protocol: "openai-chat".into(), auth: "none".into(), local: true },
    ];
    // Open via the palette command.
    run_palette_command(&mut s, PaletteCommand::Provider);
    assert!(matches!(s.overlay, Overlay::ProviderPicker { .. }));
    // Filter to "ollama", then submit → stages it.
    if let Overlay::ProviderPicker { query, .. } = &mut s.overlay {
        *query = "ollama".into();
    }
    submit_prompt(&mut s); // the picker submit path
    assert_eq!(s.pending_provider.as_deref(), Some("ollama"));
    assert!(matches!(s.overlay, Overlay::None));
}
```

(Match the test to the real `run_palette_command`/`submit_prompt` helper names and signatures at `reduce.rs:1115`/`reduce.rs:1044`; if `submit_prompt` is private, drive it through the same `Action::InputSubmit` path the model-picker test uses.)

- [ ] **Step 5: Run to verify it fails**

Run: `cargo test -p codypendent-tui provider_picker_opens_filters_and_stages`
Expected: FAIL — the reducer has no `ProviderPicker` handling.

- [ ] **Step 6: Implement the reducer arms** (mirroring the `ModelPicker` arms)

- `run_palette_command` (`reduce.rs:1128`): add a `PaletteCommand::Provider` arm resetting `selected_provider = 0` and setting `overlay = Overlay::ProviderPicker { query: String::new(), selected: 0 }`.
- `nav` (`reduce.rs:655`): add an `Overlay::ProviderPicker` arm stepping `selected` over `filter_providers(&state.providers, query).len()`, resolving `state.selected_provider = indices.get(selected).copied().unwrap_or(0)`.
- `edit_prompt` (`reduce.rs:955`): add the `Overlay::ProviderPicker` branch resetting `selected = 0` and re-resolving `selected_provider` on query change.
- `submit_prompt` (`reduce.rs:1068`): add an `Overlay::ProviderPicker { query, selected }` arm that re-derives the filtered list, sets `state.pending_provider = Some(state.providers[idx].id.clone())`, emits a `"provider staged: {id} — applies to your next run"` notice, and closes the overlay. `Esc` (`input_cancel`, `reduce.rs:989`) closes without staging (add the `ProviderPicker` variant to whatever the cancel arm matches).

- [ ] **Step 7: Seed the catalog in the CLI**

In `crates/cli/src/tui.rs`, near where `state.models = load_model_cards(paths).await;` is set (`tui.rs:166`), add `state.providers = load_provider_cards(paths);` and implement (beside `load_model_cards`, `tui.rs:1212`):

```rust
/// Seed the provider-catalog projection: the built-in catalog layered with the
/// user's `<data_dir>/providers.toml`. Never fails the TUI — a parse error
/// degrades to the built-ins with a stderr note.
fn load_provider_cards(paths: &RuntimePaths) -> Vec<ProviderCard> {
    use codypendent_providers::{AuthMethod, Catalog};
    let providers_path = paths.data_dir.join("providers.toml");
    let catalog = match Catalog::load_with_user_overrides(&providers_path) {
        Ok(catalog) => catalog,
        Err(error) => {
            eprintln!("codypendent: provider catalog fell back to built-ins ({error})");
            Catalog::builtin()
        }
    };
    catalog
        .providers()
        .map(|p| ProviderCard {
            id: p.id.clone(),
            name: p.name.clone(),
            protocol: format!("{:?}", p.protocol).to_lowercase(),
            auth: match p.auth.first() {
                Some(AuthMethod::None) | None => "none".to_string(),
                Some(AuthMethod::ApiKey { env, .. }) => format!("api-key: {}", env.first().map(String::as_str).unwrap_or("")),
                Some(AuthMethod::Acp { command, .. }) => format!("acp: {command}"),
                Some(AuthMethod::CloudIam { variant, .. }) => format!("cloud-iam: {variant}"),
                Some(AuthMethod::OAuth { .. }) => "oauth".to_string(),
            },
            local: p.local,
        })
        .collect()
}
```

Add `codypendent-providers = { workspace = true }` to `crates/cli/Cargo.toml` `[dependencies]`, and import `ProviderCard` where `ModelCard` is imported. (The rendering of the overlay list reuses the model-picker's render path in `crates/tui/src/render.rs` — add a `ProviderPicker` arm that lists `id · name · protocol · auth`, mirroring the `ModelPicker` render; a render test is optional but the reducer test above is the contract.)

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test -p codypendent-tui provider_picker_opens_filters_and_stages filters_to_the_provider_picker_command`
Expected: PASS.

- [ ] **Step 9: Full gate + commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
git add crates/tui/src/palette.rs crates/tui/src/state.rs crates/tui/src/reduce.rs crates/tui/src/render.rs crates/cli/Cargo.toml crates/cli/src/tui.rs
git commit -m "feat(tui): /provider picker over the built-in provider catalog

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## After all tasks

- Whole-branch adversarial review (per subagent-driven-development): back-compat (an existing `models.toml` still resolves + runs — Task 4 preserves `MissingApiKeyEnv`/`UnsupportedProvider`); the classification hard-filter is untouched (the picker only stages `pending_provider`; the executor's `validate_pin`/`select` gate is unchanged); T1/T7 (`ModelUsage.cost_micros` stays `None`; catalog cost is display-only); no secret value is ever stored (only env NAMEs); `cargo deny` clean with the new dep.
- Push (CodeHalwell account, restore synextra per the GitHub push procedure) and open a PR to `main`, left for the user's review.
- To observe the ACP client end-to-end against a real agent (e.g. `gemini --acp`) or wire a catalog-selected provider into a live run, see the out-of-scope follow-ups below — those need the picker auth state machine.

## Out-of-scope follow-ups (noted, NOT planned here)

- **Cloud-IAM signing:** AWS SigV4, GCP ADC token exchange, Azure Entra token refresh — the `CloudIam` credential is a trait-shaped stub returning `NotWired`; real signing/refresh is sequenced follow-ups (research: SigV4 hardest; Bedrock bearer keys + Azure `/openai/v1/` reduce the common-case pain, already in the catalog as `ApiKey`).
- **Reverse-engineered subscription OAuth** (ChatGPT/Claude/Copilot): reserved `OAuth` variant only; no reverse-engineered flow ships (ToS/ban risk — ACP is the sanctioned substitute).
- **Native non-OpenAI protocol wiring:** Anthropic Messages / Gemini `generateContent` / Bedrock Converse client construction (`client_for` returns `ProtocolNotWired` for these today; the Anthropic client is already a workspace dep behind `provider-anthropic`).
- **Picker auth state-machine polish + live ACP/hosted run wiring:** consuming `pending_provider` in an executor run (selecting a catalog provider — including spawning an `AcpClient` to drive a real run and fan its mapped events into the run ledger via the approval broker), and the connect/auth UX. The `AcpClient` + mapping (Tasks 6-7) are the foundation this builds on.
