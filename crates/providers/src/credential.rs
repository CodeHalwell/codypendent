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
#[derive(Clone, PartialEq, Eq)]
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

// `Debug` is hand-written to REDACT the key `value` — a derived `Debug` would
// print the secret, so a stray `debug!("{cred:?}")` anywhere downstream would
// leak it into logs. The header/prefix (non-secret) stay visible for diagnosis.
impl std::fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("ResolvedCredential::None"),
            Self::ApiKey { header, prefix, .. } => f
                .debug_struct("ResolvedCredential::ApiKey")
                .field("header", header)
                .field("prefix", prefix)
                .field("value", &"<redacted>")
                .finish(),
        }
    }
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
        Err(CredentialError::NotWired {
            method: "cloud-iam",
        })
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
        AuthMethod::ApiKey {
            env,
            header,
            prefix,
        } => Box::new(ApiKeyCredential {
            env: env.clone(),
            header: header.clone(),
            prefix: prefix.clone(),
        }),
        AuthMethod::CloudIam { .. } => Box::new(CloudIamCredential),
        AuthMethod::OAuth { .. } => Box::new(OAuthCredential),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AuthMethod;

    #[test]
    fn debug_redacts_the_api_key_value() {
        let cred = ResolvedCredential::ApiKey {
            header: "Authorization".to_string(),
            prefix: "Bearer ".to_string(),
            value: "sk-secret-12345".to_string(),
        };
        let dbg = format!("{cred:?}");
        assert!(
            !dbg.contains("sk-secret-12345"),
            "the key value must never appear in Debug: {dbg}"
        );
        assert!(dbg.contains("<redacted>"));
        assert!(dbg.contains("Authorization")); // non-secret header stays visible
    }

    #[tokio::test]
    async fn api_key_resolves_the_first_set_env_var() {
        // A deliberately unique name that IS set for this test only.
        let var = "CODYPENDENT_TEST_PROVIDERS_KEY_7c1f";
        std::env::set_var(var, "sk-secret");
        let auth = AuthMethod::ApiKey {
            env: vec![
                "CODYPENDENT_TEST_PROVIDERS_UNSET_a1".to_string(),
                var.to_string(),
            ],
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
        let err = credential_for(&auth)
            .resolve()
            .await
            .expect_err("must error");
        match &err {
            CredentialError::MissingEnv { var: v } => assert_eq!(v, var),
            other => panic!("expected MissingEnv, got {other:?}"),
        }
        assert!(err.to_string().contains(var), "message names the variable");
    }

    #[tokio::test]
    async fn none_and_acp_resolve_to_no_credential() {
        assert_eq!(
            credential_for(&AuthMethod::None).resolve().await.unwrap(),
            ResolvedCredential::None
        );
        let acp = AuthMethod::Acp {
            command: "gemini".into(),
            args: vec!["--acp".into()],
            env: Default::default(),
        };
        assert_eq!(
            credential_for(&acp).resolve().await.unwrap(),
            ResolvedCredential::None
        );
    }

    #[tokio::test]
    async fn cloud_iam_and_oauth_are_not_wired() {
        let cloud = AuthMethod::CloudIam {
            variant: "aws_sigv4".into(),
            env: Default::default(),
            scopes: vec![],
        };
        assert!(matches!(
            credential_for(&cloud).resolve().await,
            Err(CredentialError::NotWired { .. })
        ));
        let oauth = AuthMethod::OAuth {
            authorize_url: "x".into(),
            token_url: "y".into(),
            client_id: "z".into(),
            scopes: vec![],
            pkce: true,
        };
        assert!(matches!(
            credential_for(&oauth).resolve().await,
            Err(CredentialError::NotWired { .. })
        ));
    }
}
