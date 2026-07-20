//! Webhook listener configuration.
//!
//! The listener is opt-in and binds to loopback by default. Configuration is a
//! small TOML file; an absent file yields `Ok(None)` so callers may probe
//! conventional locations without pre-checking existence.

use std::path::Path;

use serde::Deserialize;

use super::WebhookError;

/// Configuration for the webhook listener.
#[derive(Clone, Deserialize)]
pub struct WebhooksConfig {
    /// Whether the listener is enabled. Disabled by default — a webhook endpoint
    /// is never opened unless explicitly turned on.
    #[serde(default)]
    pub enabled: bool,
    /// The address to bind. Defaults to `127.0.0.1:8765` (loopback only).
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    /// The HMAC shared secret used to verify `X-Hub-Signature-256`.
    ///
    /// This value is sensitive: it MUST NEVER be logged, emitted in traces, or
    /// stored in model context or the database.
    #[serde(default)]
    pub secret: Option<String>,
}

/// Manual `Debug` so a `{:?}` of the config (or any struct embedding it) can
/// never print the shared secret — the same treatment `GitHubToken` gets.
impl std::fmt::Debug for WebhooksConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhooksConfig")
            .field("enabled", &self.enabled)
            .field("listen_addr", &self.listen_addr)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// The default bind address: loopback, so nothing is exposed off-host.
fn default_listen_addr() -> String {
    "127.0.0.1:8765".to_string()
}

impl Default for WebhooksConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_addr: default_listen_addr(),
            secret: None,
        }
    }
}

/// Load webhook configuration from a TOML file.
///
/// Returns `Ok(None)` when the file does not exist. A parse failure is mapped to
/// [`WebhookError::Config`]; any other read failure to [`WebhookError::Io`].
pub fn load(path: &Path) -> Result<Option<WebhooksConfig>, WebhookError> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(WebhookError::Io(error)),
    };
    let config: WebhooksConfig =
        toml::from_str(&contents).map_err(|error| WebhookError::Config(error.to_string()))?;
    Ok(Some(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled_on_loopback() {
        let config = WebhooksConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.listen_addr, "127.0.0.1:8765");
        assert!(config.secret.is_none());
    }

    #[test]
    fn load_missing_file_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("no-such-file.toml");
        assert!(load(&path).expect("load").is_none());
    }

    #[test]
    fn parses_small_toml() {
        let config: WebhooksConfig =
            toml::from_str("enabled = true\nlisten_addr = \"127.0.0.1:9999\"\nsecret = \"shh\"")
                .expect("parse");
        assert!(config.enabled);
        assert_eq!(config.listen_addr, "127.0.0.1:9999");
        assert_eq!(config.secret.as_deref(), Some("shh"));
    }
}
