//! The provider/auth data model: a backward-compatible superset of `models.toml`.
//!
//! Everything here is pure data — no secrets, no network. A secret is referenced
//! by env-var NAME only (`AuthMethod::ApiKey.env`); its value is read at call
//! time by a `CredentialProvider` (the crate's `credential` module, added
//! alongside the built-in catalog) and never stored here.

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
    ///
    /// Explicit rename: serde's derived kebab-case would otherwise split the
    /// "Ai" hump and emit `open-ai-chat`; the wire/TOML spelling is `openai-chat`.
    #[serde(rename = "openai-chat")]
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
        assert_eq!(
            p.base_url.as_deref(),
            Some("https://api.groq.com/openai/v1")
        );
        assert!(!p.local);
        // Header/prefix default to Authorization / "Bearer ".
        match &p.auth[0] {
            AuthMethod::ApiKey {
                env,
                header,
                prefix,
            } => {
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
            file.providers[0]
                .extra_headers
                .get("anthropic-version")
                .map(String::as_str),
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
                assert_eq!(
                    args,
                    &vec![
                        "-y".to_string(),
                        "@agentclientprotocol/claude-agent-acp".to_string()
                    ]
                );
            }
            other => panic!("expected Acp, got {other:?}"),
        }
    }
}
