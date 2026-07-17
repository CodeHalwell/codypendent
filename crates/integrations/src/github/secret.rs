//! The token broker (Phase 3 STEP 3.1).
//!
//! Personal mode uses the developer's own GitHub credential. That credential is
//! a secret and must never enter model context, logs, or the database
//! (Chapter 11). [`GitHubToken`] is therefore opaque: it does not implement
//! `Display` or `Serialize`, its manual `Debug` prints only `<redacted>`, and
//! the raw value is reachable only through [`GitHubToken::expose`], which is
//! documented for a single caller — setting the `Authorization` header.

use std::fmt;

use tokio::process::Command;

use crate::github::GitHubError;

/// An opaque GitHub token. The inner value never appears in `Debug`, is not
/// serializable, and is reachable only via [`GitHubToken::expose`].
pub struct GitHubToken(String);

impl GitHubToken {
    /// Wrap a raw token value. Prefer [`GitHubToken::discover`] in production;
    /// this exists for tests and callers that already hold a vetted token.
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// Borrow the raw token, solely to set the `Authorization` header.
    ///
    /// Callers MUST NOT log, store, serialize, or otherwise propagate the
    /// returned value. There is deliberately no other accessor.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Read the token from the `GITHUB_TOKEN` environment variable. Returns
    /// [`GitHubError::MissingToken`] if the variable is unset or empty.
    pub fn from_env() -> Result<GitHubToken, GitHubError> {
        match std::env::var("GITHUB_TOKEN") {
            Ok(value) if !value.trim().is_empty() => Ok(GitHubToken(value.trim().to_string())),
            _ => Err(GitHubError::MissingToken("GITHUB_TOKEN".to_string())),
        }
    }

    /// Read the token by shelling out to `gh auth token`. Returns an error if
    /// the `gh` CLI is absent, exits non-zero, or prints nothing.
    pub async fn from_gh_cli() -> Result<GitHubToken, GitHubError> {
        let output = Command::new("gh").args(["auth", "token"]).output().await?;
        if !output.status.success() {
            return Err(GitHubError::MissingToken("gh auth token".to_string()));
        }
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if token.is_empty() {
            return Err(GitHubError::MissingToken("gh auth token".to_string()));
        }
        Ok(GitHubToken(token))
    }

    /// Discover a token the way the Phase 3 docs prescribe: prefer the `gh` CLI
    /// (so the daemon inherits the developer's existing session), then fall back
    /// to `GITHUB_TOKEN`.
    pub async fn discover() -> Result<GitHubToken, GitHubError> {
        match Self::from_gh_cli().await {
            Ok(token) => Ok(token),
            Err(_) => Self::from_env(),
        }
    }
}

impl fmt::Debug for GitHubToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitHubToken(\"<redacted>\")")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_reveals_the_secret() {
        let token = GitHubToken::new("ghp_SUPERSECRETVALUE");
        let rendered = format!("{token:?}");
        assert!(
            !rendered.contains("ghp_SUPERSECRETVALUE"),
            "Debug leaked the token value: {rendered}"
        );
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn expose_returns_the_raw_value() {
        let token = GitHubToken::new("ghp_abc");
        assert_eq!(token.expose(), "ghp_abc");
    }
}
