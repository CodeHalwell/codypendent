//! Explicit ACP setup: install a pinned adapter, complete its handshake, then
//! persist profiles using the shared models-file transaction.
use crate::models::ModelConfig;
use anyhow::{bail, Context};
use codypendent_integrations::{
    acp_client::{AcpClient, AcpSessionOptions},
    acp_registry::{self, AcpRegistryStore},
};
use codypendent_protocol::ModelId;
use std::path::Path;

pub struct AcpProbe {
    pub registry_id: String,
    pub version: String,
    pub models: Vec<String>,
}
impl AcpProbe {
    pub fn coordinate(&self, model: Option<&str>) -> String {
        match model {
            Some(model) => {
                acp_registry::agent_coordinate_with_model(&self.registry_id, &self.version, model)
            }
            None => acp_registry::agent_coordinate(&self.registry_id, &self.version),
        }
    }
}

/// Setup is an explicit user action, never part of passive model readiness.
/// Community adapters require the host's trust confirmation even if installed.
pub async fn probe_agent(
    data_dir: &Path,
    provider_id: &str,
    repository: &Path,
    accept_community_risk: bool,
) -> anyhow::Result<AcpProbe> {
    let id = acp_registry::agent_id_from_coordinate(provider_id);
    if id == "antigravity-community" && !accept_community_risk {
        bail!("The community Antigravity bridge requires explicit acceptance of its third-party account and terms-of-service risk");
    }
    let repository = repository
        .canonicalize()
        .context("choose an existing repository before connecting an agent")?;
    if !repository.is_dir() {
        bail!("ACP repository must be a directory");
    }
    let store = AcpRegistryStore::new(data_dir);
    if acp_registry::local_acp_agent_spec(&id).is_none()
        && acp_registry::community_acp_agent(&id).is_none()
    {
        store.load_or_refresh().await?;
    }
    let spec = match store.launch_spec(provider_id) {
        Ok(spec) => spec,
        Err(_) => store.install(provider_id, false).await?,
    };
    let client = AcpClient::spawn_with(
        &spec.command.to_string_lossy(),
        &spec.args,
        &spec.env,
        &repository.to_string_lossy(),
        AcpSessionOptions::for_agent(&spec),
    )
    .await
    .with_context(|| {
        format!(
            "ACP handshake with `{provider_id}` failed; check the agent's installation and sign-in"
        )
    })?;
    let models = client
        .discovered_models()
        .into_iter()
        .map(|model| model.id)
        .collect();
    drop(client);
    Ok(AcpProbe {
        registry_id: spec.registry_id,
        version: spec.version,
        models,
    })
}

pub fn replace_profile_family(
    data_dir: &Path,
    base: &str,
    profiles: &[(String, String)],
) -> anyhow::Result<()> {
    if base.is_empty() || base.len() > 256 || base.contains(['\0', '\n', '\r']) {
        bail!("invalid ACP profile id");
    }
    let family_prefix = format!("{base}#");
    for (profile, _) in profiles {
        if profile.is_empty()
            || profile.len() > 256
            || profile.contains(['\0', '\n', '\r'])
            || (profile != base && !profile.starts_with(&family_prefix))
        {
            bail!("invalid ACP profile id");
        }
    }
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join("models.toml");
    crate::models_file::update_model_entries(&path, |configs| {
        if let Some(conflict) = configs.iter().find(|config| {
            config.provider != "acp" && profiles.iter().any(|(profile, _)| config.id.0 == *profile)
        }) {
            bail!(
                "ACP profile `{}` conflicts with an existing {} model",
                conflict.id.0,
                conflict.provider
            );
        }
        configs.retain(|config| {
            !(config.provider == "acp"
                && (config.id.0 == base || config.id.0.starts_with(&family_prefix)))
        });
        configs.extend(profiles.iter().map(|(profile, agent)| ModelConfig {
            id: ModelId(profile.clone()),
            provider: "acp".to_string(),
            base_url: String::new(),
            model: agent.clone(),
            api_key_env: String::new(),
            context_tokens: None,
            // An ACP agent is launched, not addressed over HTTP, so it has no
            // catalog provider whose auth header would need resolving.
            provider_id: None,
        }));
        Ok(())
    })
}
