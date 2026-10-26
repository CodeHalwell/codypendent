use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use codypendent_integrations::acp_client::{AcpClient, AcpSessionOptions};
use codypendent_integrations::acp_registry::{self, AcpLaunchSpec};
use codypendent_protocol::ModelId;
use codypendent_providers::{Catalog, Protocol, Provider};
use codypendent_runtime::auth::AuthStore;
use codypendent_runtime::models::{load_models, ModelConfig};
use codypendent_runtime::models_file::update_model_entries;
use serde::Serialize;

static DISCOVERY_CACHE: tokio::sync::Mutex<Option<CachedDiscovery>> =
    tokio::sync::Mutex::const_new(None);
const RESPONSE_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct DetectedProvider {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub state: String,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ModelDiscovery {
    pub added: Vec<String>,
    pub default_model: Option<String>,
    pub detected: Vec<DetectedProvider>,
    pub warnings: Vec<String>,
    pub available_default_profiles: Vec<String>,
    #[serde(skip)]
    families: Vec<DiscoveredFamily>,
}

struct DiscoveryBatch {
    provider: DetectedProvider,
    models: Vec<ModelConfig>,
    family: Option<DiscoveredFamily>,
}

#[derive(Clone, Debug)]
enum DiscoveredFamily {
    Agent {
        id: String,
        selectors: Option<Vec<String>>,
    },
    Endpoint {
        provider_id: String,
        endpoint: String,
        models: Vec<String>,
        complete: bool,
    },
}

struct CachedDiscovery {
    completed: Instant,
    data_dir: PathBuf,
    repository: Option<PathBuf>,
    report: ModelDiscovery,
}

pub async fn discover_models(repository: Option<&Path>) -> anyhow::Result<ModelDiscovery> {
    let requested = Instant::now();
    let data_dir = super::data_dir()?;
    let mut cache = DISCOVERY_CACHE.lock().await;
    if let Some(previous) = &*cache {
        if previous.completed >= requested
            && previous.data_dir == data_dir
            && previous.repository.as_deref() == repository
        {
            return Ok(previous.report.clone());
        }
    }
    let report = discover_models_in(&data_dir, repository).await?;
    *cache = Some(CachedDiscovery {
        completed: Instant::now(),
        data_dir,
        repository: repository.map(Path::to_path_buf),
        report: report.clone(),
    });
    Ok(report)
}

async fn discover_models_in(
    data_dir: &Path,
    repository: Option<&Path>,
) -> anyhow::Result<ModelDiscovery> {
    let catalog = Catalog::load_with_user_overrides(&super::providers_path(data_dir))?;
    let auth = AuthStore::load(data_dir)?;
    let models_path = super::models_path(data_dir);
    if models_path.exists() {
        load_models(&models_path)?;
    }
    let temporary = tempfile::tempdir().context("creating an agent discovery workspace")?;
    let repository = repository.unwrap_or(temporary.path()).to_path_buf();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()?;
    let hosted_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let store = acp_registry::AcpRegistryStore::new(data_dir);
    let specs = tokio::task::spawn_blocking(move || store.installed_agent_specs()).await?;
    let mut tasks = tokio::task::JoinSet::new();
    let mut report = ModelDiscovery::default();
    let mut candidates = Vec::new();
    for provider in catalog.providers().filter(|provider| provider.local) {
        if provider.base_url.as_deref().is_some_and(loopback_endpoint)
            && super::provider_runtime_supported(provider)
            && !super::provider_requires_key(provider)
        {
            tasks.spawn(discover_endpoint(provider.clone(), client.clone()));
        }
    }
    for spec in specs {
        tasks.spawn(discover_agent(spec, repository.clone()));
    }
    for provider in catalog.providers().filter(|provider| {
        !provider.local
            && super::provider_runtime_supported(provider)
            && super::provider_requires_key(provider)
            && super::provider_has_resolvable_key(
                &provider.id,
                &auth,
                &super::provider_key_envs(provider),
            )
    }) {
        let key = auth
            .get(&super::provider_auth_id(&provider.id))
            .filter(|key| !key.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| {
                super::provider_env_in_use(provider).and_then(|name| std::env::var(name).ok())
            });
        if let Some(key) = key {
            tasks.spawn(discover_hosted(
                provider.clone(),
                hosted_client.clone(),
                key,
            ));
        }
    }
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(batch) => {
                report.detected.push(batch.provider);
                candidates.extend(batch.models);
                report.families.extend(batch.family);
            }
            Err(error) => report
                .warnings
                .push(format!("A provider scan could not finish: {error}")),
        }
    }
    candidates.sort_by_key(model_preference);
    if !candidates.is_empty() || models_path.exists() {
        (
            report.added,
            report.default_model,
            report.available_default_profiles,
        ) = persist_candidates(&models_path, candidates, &report.families)?;
    }
    report.detected.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_deadlines_allow_known_slow_agents_without_delaying_other_families() {
        for agent in [
            "claude-acp",
            "claude-code",
            "claude",
            "gemini",
            "gemini-cli",
        ] {
            assert_eq!(agent_discovery_timeout(agent), Duration::from_secs(45));
        }
        for agent in ["codex-acp", "copilot", "opencode", "custom-claude-server"] {
            assert_eq!(agent_discovery_timeout(agent), Duration::from_secs(8));
        }
    }

    fn config(id: &str, model: &str) -> ModelConfig {
        ModelConfig {
            id: ModelId(id.into()),
            provider: "openai-compatible".into(),
            base_url: "http://localhost:11434/v1".into(),
            model: model.into(),
            api_key_env: "EXISTING_KEY".into(),
            context_tokens: Some(32000),
            provider_id: Some("ollama".into()),
        }
    }

    #[test]
    fn discovered_models_preserve_existing_names_credentials_and_other_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("models.toml");
        std::fs::write(&path, "# keep this\n[speech]\nvoice = 'my-voice'\n").unwrap();
        let existing = config("my-model", "coder:latest");
        codypendent_runtime::models_file::write_model_entries(&path, &[existing.clone()]).unwrap();
        let candidates = vec![
            config("ollama/coder", "coder:latest"),
            config("new-model", "chat:latest"),
            config("my-model", "different"),
        ];
        let (added, default, _) = persist_candidates(&path, candidates.clone(), &[]).unwrap();
        assert_eq!(added, vec!["new-model"]);
        assert_eq!(default.as_deref(), Some("my-model"));
        let saved = load_models(&path).unwrap();
        assert_eq!(saved[0], existing);
        assert_eq!(saved.len(), 2);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this"));
        assert!(text.contains("voice = 'my-voice'"));
        assert!(persist_candidates(&path, candidates, &[])
            .unwrap()
            .0
            .is_empty());
    }

    #[test]
    fn local_discovery_validates_loopback_and_omits_non_chat_models() {
        for url in [
            "http://localhost:1234/v1",
            "http://127.0.0.1:8000/v1",
            "http://[::1]:11434/v1",
        ] {
            assert!(loopback_endpoint(url), "{url}");
        }
        for url in [
            "https://api.example.com/v1",
            "http://localhost.example.com/v1",
            "http://user:secret@localhost/v1",
            "http://localhost/v1?key=secret",
            "file:///tmp/models",
        ] {
            assert!(!loopback_endpoint(url), "{url}");
        }
        assert!(chat_model("qwen-coder:latest"));
        for model in [
            "nomic-embed-text",
            "text-embedding-3-large",
            "whisper-large-v3",
            "gemini-tts-preview",
            "invalid\nmodel",
        ] {
            assert!(!chat_model(model));
        }
    }

    #[tokio::test]
    async fn a_real_local_catalog_is_persisted_as_usable_profiles() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let count = stream.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET /v1/models "));
            let body = r#"{"data":[{"id":"local-coder"},{"id":"nomic-embed-text"}]}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let provider: Provider = toml::from_str(&format!("id = 'local-test'\nname = 'Local test'\nprotocol = 'openai-chat'\nbase_url = 'http://{address}/v1'\nlocal = true\nauth = []")).unwrap();
        let batch = discover_endpoint(provider, reqwest::Client::new()).await;
        assert_eq!(batch.provider.state, "listed");
        assert_eq!(batch.models.len(), 1);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("models.toml");
        let (added, default, _) = persist_candidates(&path, batch.models, &[]).unwrap();
        assert_eq!(added, vec!["local-test/local-coder"]);
        assert_eq!(default.as_deref(), Some("local-test/local-coder"));
        assert_eq!(load_models(&path).unwrap()[0].model, "local-coder");
        server.join().unwrap();
    }

    fn agent_config(id: &str, coordinate: &str) -> ModelConfig {
        ModelConfig {
            id: ModelId(id.into()),
            provider: "acp".into(),
            base_url: String::new(),
            model: coordinate.into(),
            api_key_env: String::new(),
            provider_id: None,
            context_tokens: Some(12345),
        }
    }

    #[test]
    fn installed_agent_defaults_refresh_old_versions_and_repair_only_disproved_selectors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("models.toml");
        let manual = agent_config("my-pinned-copilot", "copilot@1.0.98.#auto");
        let current = vec![
            agent_config("acp/copilot", "copilot@1.0.98."),
            agent_config("acp/copilot#auto", "copilot@1.0.98.#auto"),
            agent_config("acp/copilot#retired", "copilot@1.0.98.#retired"),
            manual.clone(),
        ];
        codypendent_runtime::models_file::write_model_entries(&path, &current).unwrap();
        let families = vec![DiscoveredFamily::Agent {
            id: "copilot".into(),
            selectors: Some(vec!["auto".into()]),
        }];
        let mut candidates = vec![
            agent_config("acp/copilot#auto", "copilot#auto"),
            agent_config("acp/copilot", "copilot"),
        ];
        candidates.sort_by_key(model_preference);
        let (_, default_model, available_default_profiles) =
            persist_candidates(&path, candidates, &families).unwrap();
        let configured = load_models(&path).unwrap();
        assert_eq!(default_model.as_deref(), Some("acp/copilot"));
        assert_eq!(available_default_profiles, ["acp/copilot"]);
        assert_eq!(
            configured
                .iter()
                .find(|row| row.id.0 == "acp/copilot")
                .unwrap()
                .model,
            "copilot"
        );
        assert_eq!(
            configured
                .iter()
                .find(|row| row.id.0 == "acp/copilot")
                .unwrap()
                .context_tokens,
            Some(12345)
        );
        assert!(!configured
            .iter()
            .any(|row| row.id.0 == "acp/copilot#retired"));
        assert!(configured.contains(&manual));
        let report = ModelDiscovery {
            default_model,
            available_default_profiles,
            families,
            ..Default::default()
        };
        assert_eq!(
            reconcile_default(
                &report,
                Some(&ModelId("acp/copilot#retired".into())),
                &configured
            )
            .as_deref(),
            Some("acp/copilot")
        );
        assert_eq!(
            reconcile_default(
                &report,
                Some(&ModelId("acp/copilot#auto".into())),
                &configured
            )
            .as_deref(),
            Some("acp/copilot#auto")
        );
        assert_eq!(
            reconcile_default(&report, Some(&manual.id), &configured).as_deref(),
            Some("my-pinned-copilot")
        );
    }

    #[test]
    fn a_slow_agent_keeps_the_selected_model_and_does_not_become_a_new_default() {
        let current = agent_config("acp/claude-acp#opus", "claude-acp@0.66.0#opus");
        let report = ModelDiscovery {
            families: vec![DiscoveredFamily::Agent {
                id: "claude-acp".into(),
                selectors: None,
            }],
            ..Default::default()
        };
        assert_eq!(
            reconcile_default(&report, Some(&current.id), &[current.clone()]).as_deref(),
            Some("acp/claude-acp#opus")
        );
        assert_eq!(reconcile_default(&report, None, &[current]), None);
    }

    #[test]
    fn an_installed_agent_timeout_adds_only_its_unverified_default_profile() {
        let batch = agent_discovery_batch(
            AcpLaunchSpec {
                registry_id: "gemini".into(),
                name: "Gemini CLI".into(),
                version: "0.46.0".into(),
                command: PathBuf::from("/installed/gemini"),
                args: vec!["--acp".into()],
                env: Default::default(),
            },
            Err(
                codypendent_integrations::acp_client::AcpClientError::Handshake(
                    "Automatic discovery timed out".into(),
                ),
            ),
        );
        assert_eq!(batch.provider.state, "unavailable");
        assert!(batch.provider.detail.contains("timed out"));
        assert_eq!(batch.models.len(), 1);
        assert_eq!(batch.models[0].id.0, "acp/gemini");
        assert_eq!(batch.models[0].model, "gemini");
        assert_eq!(batch.models[0].context_tokens, None);
        let families = vec![batch.family.unwrap()];
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("models.toml");
        let (added, default_model, available) =
            persist_candidates(&path, batch.models.clone(), &families).unwrap();
        assert_eq!(added, ["acp/gemini"]);
        assert_eq!(default_model, None);
        assert!(available.is_empty());

        let selected = agent_config("acp/gemini#existing-choice", "gemini#existing-choice");
        codypendent_runtime::models_file::write_model_entries(&path, &[selected.clone()]).unwrap();
        persist_candidates(&path, batch.models, &families).unwrap();
        let configured = load_models(&path).unwrap();
        assert!(configured.contains(&selected));
        let report = ModelDiscovery {
            families,
            ..Default::default()
        };
        assert_eq!(
            reconcile_default(&report, Some(&selected.id), &configured).as_deref(),
            Some("acp/gemini#existing-choice")
        );
    }

    #[test]
    fn startup_chooses_a_connected_default_but_manual_scans_keep_the_selected_agent() {
        let selected = agent_config("acp/claude-acp#opus", "claude-acp#opus");
        let connected = agent_config("acp/codex-acp", "codex-acp");
        let configured = [selected.clone(), connected.clone()];
        let report = ModelDiscovery {
            default_model: Some(connected.id.0.clone()),
            available_default_profiles: vec![connected.id.0.clone()],
            detected: vec![DetectedProvider {
                id: "claude-acp".into(),
                name: "Claude Code".into(),
                kind: "agent".into(),
                state: "unavailable".into(),
                detail: "Timed out".into(),
            }],
            ..Default::default()
        };
        assert_eq!(
            select_default(&report, Some(&selected.id), &configured, true).as_deref(),
            Some("acp/codex-acp")
        );
        assert_eq!(
            select_default(&report, Some(&selected.id), &configured, false).as_deref(),
            Some("acp/claude-acp#opus")
        );
        assert_eq!(
            select_default(
                &ModelDiscovery::default(),
                Some(&selected.id),
                &configured,
                true
            )
            .as_deref(),
            Some("acp/claude-acp#opus")
        );
    }

    #[test]
    fn a_live_provider_list_repairs_a_retired_catalog_default() {
        let old = config("hosted/retired", "retired");
        let current = config("hosted/current", "current");
        let report = ModelDiscovery {
            default_model: Some(current.id.0.clone()),
            available_default_profiles: vec![current.id.0.clone()],
            families: vec![DiscoveredFamily::Endpoint {
                provider_id: "ollama".into(),
                endpoint: old.base_url.clone(),
                models: vec!["current".into()],
                complete: true,
            }],
            ..Default::default()
        };
        assert_eq!(
            reconcile_default(&report, Some(&old.id), &[old.clone(), current]).as_deref(),
            Some("hosted/current")
        );
        let no_metadata = ModelDiscovery::default();
        assert_eq!(
            reconcile_default(&no_metadata, Some(&old.id), &[old.clone()]).as_deref(),
            Some("hosted/retired")
        );
    }

    #[test]
    fn only_a_complete_live_list_removes_retired_automatic_profiles() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("models.toml");
        let retired = config("ollama/retired", "retired");
        let manual = config("my-retired-model", "retired");
        let current = config("ollama/current", "current");
        codypendent_runtime::models_file::write_model_entries(
            &path,
            &[retired.clone(), manual.clone()],
        )
        .unwrap();
        let family = |complete| DiscoveredFamily::Endpoint {
            provider_id: "ollama".into(),
            endpoint: current.base_url.clone(),
            models: vec!["current".into()],
            complete,
        };
        persist_candidates(&path, vec![current.clone()], &[family(false)]).unwrap();
        assert!(load_models(&path).unwrap().contains(&retired));
        let families = vec![family(true)];
        let (_, default_model, available_default_profiles) =
            persist_candidates(&path, vec![current], &families).unwrap();
        let configured = load_models(&path).unwrap();
        assert!(!configured.contains(&retired));
        assert!(configured.contains(&manual));
        let report = ModelDiscovery {
            default_model,
            available_default_profiles,
            families,
            ..Default::default()
        };
        assert_eq!(
            reconcile_default(&report, Some(&retired.id), &configured).as_deref(),
            Some("ollama/current")
        );
    }

    #[tokio::test]
    async fn hosted_discovery_uses_existing_auth_and_only_the_account_model_list() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let count = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..count]).to_lowercase();
            assert!(request.starts_with("get /v1/models "));
            assert!(request.contains("authorization: bearer fixture-only-key"));
            let body = r#"{"data":[{"id":"account-coder"},{"id":"text-embedding-3-large"}]}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let provider: Provider = toml::from_str(&format!("id = 'hosted-test'\nname = 'Hosted test'\nprotocol = 'openai-chat'\nbase_url = 'http://{address}/v1'\nlocal = false\nauth = [{{ kind = 'api_key', env = ['UNSET_DISCOVERY_TEST_KEY'] }}]")).unwrap();
        let batch =
            discover_hosted(provider, reqwest::Client::new(), "fixture-only-key".into()).await;
        assert_eq!(batch.provider.state, "listed");
        assert_eq!(batch.models.len(), 1);
        assert_eq!(batch.models[0].model, "account-coder");
        assert!(!serde_json::to_string(&batch.provider)
            .unwrap()
            .contains("fixture-only-key"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn ollama_metadata_omits_decision_and_gone_models_without_generating() {
        use std::io::{BufRead, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(&mut stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert!(line.starts_with("POST /api/show "));
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let (status, body) = match request["model"].as_str().unwrap() {
                    "decision" => ("200 OK", r#"{"capabilities":["decision"]}"#),
                    "gone" => ("410 Gone", "{}"),
                    "chat" => ("200 OK", r#"{"capabilities":["completion","tools"]}"#),
                    _ => ("200 OK", "{}"),
                };
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let names = ["decision", "gone", "chat", "unknown"]
            .map(str::to_string)
            .to_vec();
        let filtered = filter_ollama_chat_models(
            &reqwest::Client::new(),
            &format!("http://{address}/v1"),
            names,
        )
        .await;
        assert_eq!(filtered, ["chat", "unknown"]);
        server.join().unwrap();
    }

    #[tokio::test]
    #[ignore = "checks installed agents without submitting any prompts"]
    async fn installed_agents_are_discovered_without_changing_user_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let report = discover_models_in(directory.path(), None).await.unwrap();
        for provider in &report.detected {
            eprintln!(
                "{}: {} ({})",
                provider.name, provider.state, provider.detail
            );
        }
        eprintln!(
            "{} profiles; default {:?}",
            report.added.len(),
            report.default_model
        );
        assert!(
            !report.added.is_empty(),
            "this machine has installed ACP agents"
        );
        assert!(report.default_model.is_some());
        assert!(
            load_models(&directory.path().join("models.toml"))
                .unwrap()
                .len()
                > 0
        );
    }
}

fn loopback_endpoint(endpoint: &str) -> bool {
    reqwest::Url::parse(endpoint).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            })
    })
}

fn chat_model(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    !id.is_empty()
        && id.len() <= 256
        && !id.contains(['\n', '\r', '\0'])
        && ![
            "embedding",
            "embed-",
            "-embed",
            "rerank",
            "whisper",
            "transcribe",
            "speech",
            "tts",
            "dall-e",
            "image-generation",
            "moderation",
        ]
        .iter()
        .any(|term| id.contains(term))
}

fn model_preference(model: &ModelConfig) -> (u8, u8, u8, String) {
    let local = loopback_endpoint(&model.base_url);
    let rank = if model.provider == "acp" {
        0
    } else if local {
        1
    } else {
        2
    };
    let name = model.model.to_lowercase();
    let coding = !["sonnet", "codex", "coder", "code", "flash", "mini"]
        .iter()
        .any(|term| name.contains(term));
    let selected = model.provider == "acp"
        && acp_registry::agent_model_from_coordinate(&model.model).is_some();
    (
        rank,
        u8::from(selected),
        u8::from(coding),
        model.id.0.clone(),
    )
}

fn same_model(a: &ModelConfig, b: &ModelConfig) -> bool {
    a.provider == b.provider
        && a.base_url.trim_end_matches('/') == b.base_url.trim_end_matches('/')
        && a.model == b.model
}

fn persist_candidates(
    path: &Path,
    candidates: Vec<ModelConfig>,
    families: &[DiscoveredFamily],
) -> anyhow::Result<(Vec<String>, Option<String>, Vec<String>)> {
    update_model_entries(path, |current| {
        let mut added = Vec::new();
        let mut defaults = Vec::new();
        for family in families {
            let DiscoveredFamily::Agent { id, selectors } = family else {
                if let DiscoveredFamily::Endpoint {
                    provider_id,
                    models,
                    complete: true,
                    ..
                } = family
                {
                    current.retain(|row| {
                        row.provider_id.as_deref() != Some(provider_id)
                            || row.id.0 != format!("{provider_id}/{}", row.model)
                            || models.contains(&row.model)
                    });
                }
                continue;
            };
            let base = format!("acp/{id}");
            current.retain_mut(|row| {
                if row.provider != "acp"
                    || (row.id.0 != base && !row.id.0.starts_with(&format!("{base}#")))
                    || acp_registry::agent_id_from_coordinate(&row.model) != *id
                {
                    return true;
                }
                let selected =
                    acp_registry::agent_model_from_coordinate(&row.model).map(str::to_owned);
                if selected.as_ref().is_some_and(|selected| {
                    selectors
                        .as_ref()
                        .is_some_and(|known| !known.is_empty() && !known.contains(selected))
                }) {
                    return false;
                }
                row.model =
                    selected.map_or_else(|| id.clone(), |selected| format!("{id}#{selected}"));
                true
            });
        }
        for candidate in candidates {
            let eligible = candidate.provider != "acp"
                || (acp_registry::agent_model_from_coordinate(&candidate.model).is_none()
                    && !families.iter().any(|family| {
                        matches!(family, DiscoveredFamily::Agent { id, selectors: None }
                            if *id == acp_registry::agent_id_from_coordinate(&candidate.model))
                    }));
            let id = if candidate.provider == "acp" {
                if let Some(existing) = current.iter_mut().find(|row| row.id == candidate.id) {
                    if existing.provider != "acp"
                        || acp_registry::agent_id_from_coordinate(&existing.model)
                            != acp_registry::agent_id_from_coordinate(&candidate.model)
                    {
                        continue;
                    }
                    existing.model = candidate.model.clone();
                } else {
                    added.push(candidate.id.0.clone());
                    current.push(candidate.clone());
                }
                Some(candidate.id.0)
            } else if let Some(existing) = current.iter().find(|row| same_model(row, &candidate)) {
                Some(existing.id.0.clone())
            } else if !current.iter().any(|row| row.id == candidate.id) {
                added.push(candidate.id.0.clone());
                let id = candidate.id.0.clone();
                current.push(candidate);
                Some(id)
            } else {
                None
            };
            if eligible {
                if let Some(id) = id.filter(|id| !defaults.contains(id)) {
                    defaults.push(id);
                }
            }
        }
        Ok((added, defaults.first().cloned(), defaults))
    })
}

pub fn reconcile_discovery_default(
    report: &ModelDiscovery,
    pinned: Option<&ModelId>,
    prefer_available: bool,
) -> anyhow::Result<Option<ModelId>> {
    let path = super::models_path(&super::data_dir()?);
    let configured = if path.exists() {
        load_models(&path)?
    } else {
        Vec::new()
    };
    Ok(select_default(report, pinned, &configured, prefer_available).map(ModelId))
}

fn select_default(
    report: &ModelDiscovery,
    pinned: Option<&ModelId>,
    configured: &[ModelConfig],
    prefer_available: bool,
) -> Option<String> {
    if prefer_available
        && !report.available_default_profiles.is_empty()
        && pinned.is_some_and(|pinned| unavailable_preference(report, pinned, configured))
    {
        report.default_model.clone()
    } else {
        reconcile_default(report, pinned, configured)
    }
}

fn unavailable_preference(
    report: &ModelDiscovery,
    pinned: &ModelId,
    configured: &[ModelConfig],
) -> bool {
    let Some(current) = configured.iter().find(|row| row.id == *pinned) else {
        return false;
    };
    report.detected.iter().any(|provider| {
        provider.state == "unavailable"
            && if current.provider == "acp" {
                acp_registry::agent_id_from_coordinate(&current.model) == provider.id
            } else {
                current.provider_id.as_deref() == Some(provider.id.as_str())
            }
    })
}

fn reconcile_default(
    report: &ModelDiscovery,
    pinned: Option<&ModelId>,
    configured: &[ModelConfig],
) -> Option<String> {
    let Some(pinned) = pinned else {
        return report.default_model.clone();
    };
    let current = configured.iter().find(|row| row.id == *pinned);
    for family in &report.families {
        match family {
            DiscoveredFamily::Agent { id, selectors } => {
                let base = format!("acp/{id}");
                if pinned.0 != base && !pinned.0.starts_with(&format!("{base}#")) {
                    continue;
                }
                let Some(selectors) = selectors else {
                    return current.map(|_| pinned.0.clone());
                };
                let selected = current
                    .and_then(|row| acp_registry::agent_model_from_coordinate(&row.model))
                    .or_else(|| pinned.0.split_once('#').map(|(_, selected)| selected));
                if current.is_some()
                    && selected.is_none_or(|selected| {
                        selectors.is_empty() || selectors.iter().any(|known| known == selected)
                    })
                {
                    return Some(pinned.0.clone());
                }
                return report
                    .available_default_profiles
                    .contains(&base)
                    .then_some(base);
            }
            DiscoveredFamily::Endpoint {
                provider_id,
                endpoint,
                models,
                complete,
            } => {
                let belongs = current.map_or_else(
                    || pinned.0.starts_with(&format!("{provider_id}/")),
                    |current| {
                        current.provider_id.as_deref() == Some(provider_id)
                            || current.base_url.trim_end_matches('/')
                                == endpoint.trim_end_matches('/')
                    },
                );
                if !belongs {
                    continue;
                }
                if current.is_some_and(|current| !complete || models.contains(&current.model)) {
                    return Some(pinned.0.clone());
                }
                return report
                    .available_default_profiles
                    .iter()
                    .find(|id| {
                        configured.iter().any(|row| {
                            row.id.0 == **id && row.provider_id.as_deref() == Some(provider_id)
                        })
                    })
                    .cloned()
                    .or_else(|| report.default_model.clone());
            }
        }
    }
    current
        .map(|_| pinned.0.clone())
        .or_else(|| report.default_model.clone())
}

struct EndpointModels {
    models: Vec<String>,
    complete: bool,
}

async fn endpoint_models(
    client: &reqwest::Client,
    endpoint: &str,
) -> anyhow::Result<EndpointModels> {
    listed_models(client.get(format!("{}/models", endpoint.trim_end_matches('/')))).await
}

async fn response_json(mut response: reqwest::Response) -> anyhow::Result<serde_json::Value> {
    if response
        .content_length()
        .is_some_and(|size| size > RESPONSE_LIMIT as u64)
    {
        bail!("model list exceeded the discovery limit");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
            bail!("model list exceeded the discovery limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

async fn listed_models(request: reqwest::RequestBuilder) -> anyhow::Result<EndpointModels> {
    let response = request.send().await.map_err(reqwest::Error::without_url)?;
    let response = response
        .error_for_status()
        .map_err(reqwest::Error::without_url)?;
    let value = response_json(response).await?;
    let rows = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(serde_json::Value::as_array)
        .context("endpoint did not return a model list")?;
    let complete = rows.len() <= 256
        && value.get("has_more").and_then(serde_json::Value::as_bool) != Some(true)
        && ["next_page_token", "nextPageToken", "next_cursor"]
            .iter()
            .all(|key| {
                value
                    .get(key)
                    .is_none_or(|value| value.is_null() || value.as_str() == Some(""))
            });
    Ok(EndpointModels {
        complete,
        models: rows
            .iter()
            .take(256)
            .filter_map(|row| row.get("id").or_else(|| row.get("name"))?.as_str())
            .map(|id| id.strip_prefix("models/").unwrap_or(id))
            .filter(|id| chat_model(id))
            .map(ToOwned::to_owned)
            .collect(),
    })
}

async fn filter_ollama_chat_models(
    client: &reqwest::Client,
    endpoint: &str,
    models: Vec<String>,
) -> Vec<String> {
    let Ok(mut url) = reqwest::Url::parse(endpoint) else {
        return models;
    };
    let path = url
        .path()
        .trim_end_matches('/')
        .strip_suffix("/v1")
        .unwrap_or(url.path().trim_end_matches('/'));
    let path = format!("{path}/api/show");
    url.set_path(&path);
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(4));
    let mut requests = tokio::task::JoinSet::new();
    for model in &models {
        let (client, url, model, semaphore) = (
            client.clone(),
            url.clone(),
            model.clone(),
            semaphore.clone(),
        );
        requests.spawn(async move {
            let _permit = semaphore.acquire().await.ok()?;
            let response = client
                .post(url)
                .json(&serde_json::json!({"model": model}))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .ok()?;
            if matches!(response.status().as_u16(), 404 | 410) {
                return Some(model);
            }
            let metadata = response_json(response.error_for_status().ok()?)
                .await
                .ok()?;
            let capabilities = metadata.get("capabilities")?.as_array()?;
            (!capabilities
                .iter()
                .any(|capability| matches!(capability.as_str(), Some("completion" | "chat"))))
            .then_some(model)
        });
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut unsupported = Vec::new();
    while let Ok(Some(result)) = tokio::time::timeout_at(deadline, requests.join_next()).await {
        if let Ok(Some(model)) = result {
            unsupported.push(model);
        }
    }
    models
        .into_iter()
        .filter(|model| !unsupported.contains(model))
        .collect()
}

async fn discover_endpoint(provider: Provider, client: reqwest::Client) -> DiscoveryBatch {
    let endpoint = provider.base_url.as_deref().unwrap_or_default();
    let (models, state, detail, complete) = match endpoint_models(&client, endpoint).await {
        Ok(listing) => {
            let complete = listing.complete;
            let names = if provider.id == "ollama" {
                filter_ollama_chat_models(&client, endpoint, listing.models).await
            } else {
                listing.models
            };
            let state = if names.is_empty() { "empty" } else { "listed" };
            let detail = format!(
                "{} installed models available on this computer.",
                names.len()
            );
            let models = names
                .into_iter()
                .map(|model| ModelConfig {
                    id: ModelId(format!("{}/{}", provider.id, model)),
                    provider: "openai-compatible".into(),
                    base_url: super::normalize_base_url(endpoint),
                    model,
                    api_key_env: String::new(),
                    provider_id: Some(provider.id.clone()),
                    context_tokens: None,
                })
                .collect();
            (models, state, detail, complete)
        }
        Err(error) => (
            Vec::new(),
            "unavailable",
            format!("Could not connect to {}: {error}", provider.name),
            false,
        ),
    };
    let family = (state != "unavailable").then(|| DiscoveredFamily::Endpoint {
        provider_id: provider.id.clone(),
        endpoint: super::normalize_base_url(endpoint),
        models: models
            .iter()
            .map(|model: &ModelConfig| model.model.clone())
            .collect(),
        complete,
    });
    DiscoveryBatch {
        provider: DetectedProvider {
            id: provider.id,
            name: provider.name,
            kind: "local".into(),
            state: state.into(),
            detail,
        },
        models,
        family,
    }
}

async fn discover_hosted(
    provider: Provider,
    client: reqwest::Client,
    key: String,
) -> DiscoveryBatch {
    let endpoint = provider.base_url.as_deref().unwrap_or_default();
    let mut request = client.get(format!("{}/models", endpoint.trim_end_matches('/')));
    if let Some(codypendent_providers::AuthMethod::ApiKey { header, prefix, .. }) =
        provider.auth.first()
    {
        request = request.header(header.as_str(), format!("{prefix}{key}"));
    }
    if provider.protocol == Protocol::Anthropic {
        request = request.header("anthropic-version", "2023-06-01");
    }
    let (models, state, detail, family) = match listed_models(request).await {
        Ok(listing) => {
            let names = listing.models;
            let detail = format!("{} models listed for your existing sign-in.", names.len());
            let family = Some(DiscoveredFamily::Endpoint {
                provider_id: provider.id.clone(),
                endpoint: super::normalize_base_url(endpoint),
                models: names.clone(),
                complete: listing.complete,
            });
            let state = if names.is_empty() { "empty" } else { "listed" };
            let models = names
                .into_iter()
                .map(|model| ModelConfig {
                    id: ModelId(format!("{}/{}", provider.id, model)),
                    provider: "openai-compatible".into(),
                    base_url: super::normalize_base_url(endpoint),
                    model,
                    api_key_env: super::provider_env_in_use(&provider).unwrap_or_default(),
                    provider_id: Some(provider.id.clone()),
                    context_tokens: None,
                })
                .collect();
            (models, state, detail, family)
        }
        Err(error) => (
            Vec::new(),
            "unavailable",
            format!(
                "Existing credentials found, but {} could not list your models: {error}",
                provider.name
            ),
            None,
        ),
    };
    DiscoveryBatch {
        provider: DetectedProvider {
            id: provider.id,
            name: provider.name,
            kind: "hosted".into(),
            state: state.into(),
            detail,
        },
        models,
        family,
    }
}

fn agent_discovery_timeout(agent: &str) -> Duration {
    match acp_registry::canonical_agent_id(agent).as_str() {
        "claude-acp" | "gemini" => Duration::from_secs(45),
        _ => Duration::from_secs(8),
    }
}

async fn discover_agent(mut spec: AcpLaunchSpec, repository: PathBuf) -> DiscoveryBatch {
    spec.env.insert("CI".into(), "true".into());
    spec.env.insert("NO_COLOR".into(), "1".into());
    let result = tokio::time::timeout(agent_discovery_timeout(&spec.registry_id), AcpClient::spawn_with(
        &spec.command.to_string_lossy(),
        &spec.args,
        &spec.env,
        &repository.to_string_lossy(),
        AcpSessionOptions::for_agent(&spec),
    )).await.unwrap_or_else(|_| Err(codypendent_integrations::acp_client::AcpClientError::Handshake("Background discovery timed out; the next task retries with this agent's existing sign-in.".into())));
    agent_discovery_batch(spec, result)
}

fn agent_discovery_batch(
    spec: AcpLaunchSpec,
    result: Result<AcpClient, codypendent_integrations::acp_client::AcpClientError>,
) -> DiscoveryBatch {
    let (models, state, detail, selectors) = match result {
        Ok(client) => {
            let base = format!("acp/{}", spec.registry_id);
            let coordinate = acp_registry::agent_id_from_coordinate(&spec.registry_id);
            let discovered: Vec<_> = client
                .discovered_models()
                .into_iter()
                .filter(|model| chat_model(&model.id))
                .map(|model| model.id)
                .collect();
            let mut profiles = vec![(base.clone(), coordinate.clone())];
            profiles.extend(
                discovered
                    .iter()
                    .map(|model| (format!("{base}#{model}"), format!("{coordinate}#{model}"))),
            );
            drop(client);
            let models = profiles
                .into_iter()
                .map(|(id, model)| ModelConfig {
                    id: ModelId(id),
                    provider: "acp".into(),
                    base_url: String::new(),
                    model,
                    api_key_env: String::new(),
                    context_tokens: None,
                    provider_id: None,
                })
                .collect();
            (
                models,
                "connected",
                "Installed agent connected. New tasks can use its default model and existing sign-in.".to_string(),
                Some(discovered),
            )
        }
        Err(error) => (
            vec![ModelConfig {
                id: ModelId(format!("acp/{}", spec.registry_id)),
                provider: "acp".into(),
                base_url: String::new(),
                model: acp_registry::agent_id_from_coordinate(&spec.registry_id),
                api_key_env: String::new(),
                context_tokens: None,
                provider_id: None,
            }],
            "unavailable",
            format!("Installed agent {} could not connect: {error}", spec.name),
            None,
        ),
    };
    let family = Some(DiscoveredFamily::Agent {
        id: spec.registry_id.clone(),
        selectors,
    });
    DiscoveryBatch {
        provider: DetectedProvider {
            id: spec.registry_id,
            name: spec.name,
            kind: "agent".into(),
            state: state.into(),
            detail,
        },
        models,
        family,
    }
}
