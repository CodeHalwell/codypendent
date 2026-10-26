//! The assembly's [`ModelProbeGateway`] — the daemon side of `ProbeModel`.
//!
//! The verdicts are the ones the TUI's model picker and the desktop's Models
//! page already computed in-process, moved to the daemon so every client shares
//! one answer: credentials are resolved without contacting providers, explicit
//! Test requests may contact their endpoint, and an ACP agent is left
//! to the check the executor already runs when a run starts.
//!
//! # Why the failure is classified
//!
//! An unavailable model is exactly where a client wants to offer the right next
//! step, and it is the same question a failed run asks. So the verdict carries
//! the same [`CodypendentError`] a failed run does, produced by the same
//! [`classify_run_failure`](codypendent_runtime::models::classify_run_failure)
//! — one classification, not two that drift.

use std::path::Path;
use std::sync::Arc;

use codypendent_daemon::models::{
    model_not_configured, ModelProbeFuture, ModelProbeGateway, ProbeModelRequest,
};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{CodypendentError, ModelProbe, ModelReadiness};
use codypendent_runtime::models::{load_models, ModelConfig, ModelRegistry};

/// The host:port a person recognises, for the `Ready` sentence.
fn endpoint_host(url: &str) -> &str {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(url)
}

/// One model's verdict.
async fn probe_one(registry: &ModelRegistry, config: &ModelConfig, network: bool) -> ModelProbe {
    let (readiness, probed) = if config.provider == "acp" {
        (
            ModelReadiness::Unverified {
                detail: "an ACP agent is checked by the daemon when a run starts (installed, \
                         launchable, handshake)"
                    .to_string(),
            },
            false,
        )
    } else if config.base_url.trim().is_empty() {
        (
            ModelReadiness::Unavailable {
                detail: "no base URL is configured for this model".to_string(),
                error: None,
            },
            false,
        )
    // A passive refresh must never send credentials to a provider, including
    // local endpoints. Only an explicit Test authorizes a network request.
    } else if network {
        let verdict = match registry.check_model(&config.id).await {
            Ok(()) => ModelReadiness::Ready {
                detail: format!(
                    "{} answered and lists {}",
                    endpoint_host(&config.base_url),
                    config.model
                ),
            },
            Err(error) => {
                let detail = error.to_string();
                // Classify through the SAME path a failed run takes, so
                // `user_action` means the same thing in both places.
                let classified =
                    codypendent_runtime::models::classify_run_failure(&anyhow::Error::from(error));
                ModelReadiness::Unavailable {
                    detail,
                    error: classified,
                }
            }
        };
        (verdict, true)
    } else {
        let verdict = match registry.credentials_resolvable(&config.id).await {
            Ok(true) => ModelReadiness::Unverified {
                detail: "the credential resolves; a networked probe asks the provider".to_string(),
            },
            Ok(false) => ModelReadiness::Unavailable {
                detail: "no credential is configured for this model".to_string(),
                error: {
                    let mut error = CodypendentError::new(
                        "model.missing-credential",
                        format!("no credential is configured for `{}`", config.id.0),
                        false,
                    );
                    error.user_action = Some(codypendent_protocol::UserAction::Reauthenticate);
                    Some(error)
                },
            },
            Err(error) => {
                let detail = error.to_string();
                let classified =
                    codypendent_runtime::models::classify_run_failure(&anyhow::Error::from(error));
                ModelReadiness::Unavailable {
                    detail,
                    error: classified,
                }
            }
        };
        (verdict, false)
    };
    ModelProbe {
        id: config.id.clone(),
        readiness,
        probed,
    }
}

/// Load the configured models in FILE order, which is also the order the
/// executor tries them in, so a readiness list reads top-to-bottom the way a
/// run resolves. An absent file is no models, not an error: a fresh install has
/// none, and "you have configured nothing" is a legitimate answer to the
/// question a client is asking.
fn configured_models(data_dir: &Path) -> Result<Vec<ModelConfig>, CodypendentError> {
    let path = data_dir.join("models.toml");
    if !path.exists() {
        return Ok(Vec::new());
    }
    load_models(&path).map_err(|error| {
        CodypendentError::new(
            "model.config-unreadable",
            format!("could not read {}: {error}", path.display()),
            false,
        )
    })
}

/// The assembly's model-probe seam, over the daemon's own configuration.
pub struct ModelProbeOps {
    paths: RuntimePaths,
}

impl ModelProbeOps {
    #[must_use]
    pub fn new(paths: RuntimePaths) -> Self {
        Self { paths }
    }

    /// Shared by both arms: the registry a run would build.
    ///
    /// A CORRUPT `auth.json` is not an empty one. Swallowing the load error
    /// would make every hosted model report "no credential is configured",
    /// sending an operator to replace credentials that are present and merely
    /// unreadable — the same defect the desktop's readiness had, and the whole
    /// reason this probe exists is to give one honest answer.
    fn registry(&self, configs: &[ModelConfig]) -> Result<ModelRegistry, CodypendentError> {
        let auth =
            codypendent_runtime::auth::AuthStore::load(&self.paths.data_dir).map_err(|error| {
                CodypendentError::new(
                    "model.credentials-unreadable",
                    format!(
                        "could not read {}: {error}",
                        self.paths.data_dir.join("auth.json").display()
                    ),
                    false,
                )
            })?;
        let catalog = codypendent_providers::Catalog::load_with_user_overrides(
            &self.paths.data_dir.join("providers.toml"),
        )
        .unwrap_or_else(|_| codypendent_providers::Catalog::builtin());
        Ok(ModelRegistry::new(configs.to_vec())
            .with_auth(auth)
            .with_catalog(catalog))
    }

    async fn run(&self, request: ProbeModelRequest) -> Result<Vec<ModelProbe>, CodypendentError> {
        let configs = configured_models(&self.paths.data_dir)?;
        let registry = self.registry(&configs)?;
        match request.model {
            Some(id) => {
                let config = configs
                    .iter()
                    .find(|config| config.id == id)
                    .ok_or_else(|| model_not_configured(&id))?;
                Ok(vec![probe_one(&registry, config, request.network).await])
            }
            None => {
                let mut probes = Vec::with_capacity(configs.len());
                for config in &configs {
                    probes.push(probe_one(&registry, config, request.network).await);
                }
                Ok(probes)
            }
        }
    }
}

impl ModelProbeGateway for ModelProbeOps {
    fn probe(&self, request: ProbeModelRequest) -> ModelProbeFuture<'_> {
        Box::pin(self.run(request))
    }
}

/// Build the seam the executor hands the server.
#[must_use]
pub fn gateway(paths: RuntimePaths) -> Arc<dyn ModelProbeGateway> {
    Arc::new(ModelProbeOps::new(paths))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::ModelId;

    fn paths_with(models_toml: &str) -> (tempfile::TempDir, RuntimePaths) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        paths.ensure_directories().expect("directories");
        if !models_toml.is_empty() {
            std::fs::write(paths.data_dir.join("models.toml"), models_toml).expect("write");
        }
        (dir, paths)
    }

    /// A fresh install has no `models.toml`, and "nothing is configured" is a
    /// legitimate answer — not a rejection a client has to special-case.
    #[tokio::test]
    async fn no_configuration_is_an_empty_list_not_a_refusal() {
        let (_dir, paths) = paths_with("");
        let probes = ModelProbeOps::new(paths)
            .run(ProbeModelRequest {
                model: None,
                network: false,
            })
            .await
            .expect("an empty list, not an error");
        assert!(probes.is_empty());
    }

    /// Naming a model that is not configured is a refusal, so a client's
    /// per-row Test cannot silently report on nothing.
    #[tokio::test]
    async fn an_unconfigured_id_is_refused_by_code() {
        let (_dir, paths) = paths_with(
            r#"
[[model]]
id = "worker"
provider = "openai-compatible"
base_url = "https://api.openai.com/v1"
model = "gpt-5"
api_key_env = "OPENAI_API_KEY"
"#,
        );
        let error = ModelProbeOps::new(paths)
            .run(ProbeModelRequest {
                model: Some(ModelId("absent".to_string())),
                network: false,
            })
            .await
            .expect_err("no such model");
        assert_eq!(error.code, "model.not-configured");
    }

    /// The network-free arm: a hosted model with no resolvable credential is
    /// `Unavailable` and says to re-authenticate, and one that is merely
    /// unproven is `Unverified` — never `Ready`, which would be a claim the
    /// probe did not earn. Neither arm touches the network.
    #[tokio::test]
    async fn a_credential_only_probe_never_claims_ready() {
        let (_dir, paths) = paths_with(
            r#"
[[model]]
id = "worker"
provider = "openai-compatible"
base_url = "https://api.openai.com/v1"
model = "gpt-5"
api_key_env = "CODYPENDENT_TEST_KEY_THAT_IS_NOT_SET"
"#,
        );
        let probes = ModelProbeOps::new(paths)
            .run(ProbeModelRequest {
                model: None,
                network: false,
            })
            .await
            .expect("probe");
        assert_eq!(probes.len(), 1);
        let probe = &probes[0];
        assert_eq!(probe.id.0, "worker");
        assert!(!probe.probed, "the network was not asked for");
        assert!(
            probe.readiness.is_unavailable(),
            "an unset credential is not ready: {:?}",
            probe.readiness
        );
        let ModelReadiness::Unavailable { error, .. } = &probe.readiness else {
            panic!("expected Unavailable, got {:?}", probe.readiness);
        };
        let error = error.as_ref().expect("a classified cause");
        assert_eq!(
            error.user_action,
            Some(codypendent_protocol::UserAction::Reauthenticate)
        );
    }

    #[tokio::test]
    async fn a_passive_local_probe_does_not_attempt_a_connection() {
        let (_dir, paths) = paths_with(
            r#"
[[model]]
id = "local"
provider = "openai-compatible"
base_url = "http://127.0.0.1:9/v1"
model = "local-model"
api_key_env = ""
"#,
        );
        let ops = ModelProbeOps::new(paths);
        let passive = ops
            .run(ProbeModelRequest {
                model: None,
                network: false,
            })
            .await
            .unwrap();
        assert!(!passive[0].probed);
        assert!(matches!(
            passive[0].readiness,
            ModelReadiness::Unverified { .. }
        ));
    }

    /// A model with no base URL is refused without any lookup, and carries no
    /// invented classification.
    #[tokio::test]
    async fn a_model_without_an_endpoint_is_unavailable_and_unclassified() {
        let (_dir, paths) = paths_with(
            r#"
[[model]]
id = "broken"
provider = "openai-compatible"
base_url = ""
model = "gpt-5"
api_key_env = ""
"#,
        );
        let probes = ModelProbeOps::new(paths)
            .run(ProbeModelRequest {
                model: Some(ModelId("broken".to_string())),
                network: true,
            })
            .await
            .expect("probe");
        let ModelReadiness::Unavailable { detail, error } = &probes[0].readiness else {
            panic!("expected Unavailable, got {:?}", probes[0].readiness);
        };
        assert!(detail.contains("base URL"));
        assert!(error.is_none(), "nothing typed to classify");
        assert!(!probes[0].probed);
    }

    #[test]
    fn the_ready_sentence_names_the_endpoint_a_person_recognises() {
        assert_eq!(
            endpoint_host("http://localhost:11434/v1"),
            "localhost:11434"
        );
        assert_eq!(endpoint_host("https://api.openai.com/v1"), "api.openai.com");
    }

    /// A credential-only probe must NOT reach a remote host. The local
    /// shortcut is what makes that possible to get wrong: a URL whose text
    /// merely contains "localhost" is not a local endpoint, and probing it
    /// would send the configured authorization header somewhere the caller
    /// explicitly did not permit.
    #[tokio::test]
    async fn a_remote_host_that_merely_mentions_localhost_is_not_probed() {
        let (_dir, paths) = paths_with(
            r#"
[[model]]
id = "sneaky"
provider = "openai-compatible"
base_url = "https://localhost.example.com/v1"
model = "gpt-5"
api_key_env = "CODYPENDENT_TEST_KEY_THAT_IS_NOT_SET"
"#,
        );
        let probes = ModelProbeOps::new(paths)
            .run(ProbeModelRequest {
                model: None,
                network: false,
            })
            .await
            .expect("probe");
        assert!(
            !probes[0].probed,
            "a credential-only probe must not have reached the network"
        );
    }
}
