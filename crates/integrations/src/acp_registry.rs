//! Official ACP agent-registry discovery and pinned launch/install support,
//! plus explicitly consented community adapters with immutable release hashes.
//!
//! The registry is data, never authority: Codypendent validates and caches a
//! bounded copy, pins package versions exactly as published, and only installs
//! binary archives after an explicit user action. Archive extraction rejects
//! links, special files, duplicate paths, traversal, and resource bombs.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// ACP's canonical, curated registry endpoint.
pub const ACP_REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

const MAX_REGISTRY_BYTES: usize = 4 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 20_000;
const MAX_PATH_BYTES: usize = 512;
const MAX_ARGS: usize = 128;
const MAX_ENV: usize = 128;
const REGISTRY_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// The current official registry document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpRegistry {
    pub version: String,
    pub agents: Vec<AcpRegistryAgent>,
}

/// One installable ACP agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpRegistryAgent {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub repository: String,
    pub distribution: AcpDistribution,
}

/// Registry distributions are additive; an entry may offer more than one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AcpDistribution {
    #[serde(default)]
    pub npx: Option<AcpPackageDistribution>,
    #[serde(default)]
    pub uvx: Option<AcpPackageDistribution>,
    #[serde(default)]
    pub binary: BTreeMap<String, AcpBinaryDistribution>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpPackageDistribution {
    pub package: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpBinaryDistribution {
    pub archive: String,
    pub cmd: String,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// A fully resolved, version-pinned ACP subprocess launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpLaunchSpec {
    pub registry_id: String,
    pub name: String,
    pub version: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AcpRegistryError {
    #[error("could not fetch the ACP registry: {0}")]
    Fetch(String),
    #[error("ACP registry response exceeds {MAX_REGISTRY_BYTES} bytes")]
    RegistryTooLarge,
    #[error("ACP archive exceeds {MAX_ARCHIVE_BYTES} bytes")]
    ArchiveTooLarge,
    #[error("invalid ACP registry: {0}")]
    Invalid(String),
    #[error("ACP agent `{0}` is not in the cached registry; run `codypendent acp refresh`")]
    UnknownAgent(String),
    #[error("ACP agent `{agent}` has no distribution for platform `{platform}`")]
    UnsupportedPlatform { agent: String, platform: String },
    #[error("ACP agent `{agent}` is not installed; run `codypendent acp install {agent}`")]
    NotInstalled { agent: String },
    #[error("ACP package runner `{tool}` is unavailable; install it and retry")]
    ToolUnavailable { tool: String },
    #[error("ACP binary `{agent}` has no registry checksum; repeat with --allow-unverified")]
    MissingChecksum { agent: String },
    #[error("ACP archive checksum mismatch (expected {expected}, got {actual})")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("ACP registry I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("ACP archive is invalid: {0}")]
    Archive(String),
}

impl AcpRegistry {
    /// Parse and validate one bounded registry document.
    pub fn parse(bytes: &[u8]) -> Result<Self, AcpRegistryError> {
        if bytes.len() > MAX_REGISTRY_BYTES {
            return Err(AcpRegistryError::RegistryTooLarge);
        }
        let registry: Self = serde_json::from_slice(bytes)
            .map_err(|error| AcpRegistryError::Invalid(error.to_string()))?;
        registry.validate()?;
        Ok(registry)
    }

    fn validate(&self) -> Result<(), AcpRegistryError> {
        if self.version.trim().is_empty() || self.agents.len() > 1_000 {
            return Err(AcpRegistryError::Invalid(
                "blank version or too many agents".to_string(),
            ));
        }
        let mut ids = BTreeSet::new();
        for agent in &self.agents {
            if !valid_id(&agent.id) || !ids.insert(agent.id.clone()) {
                return Err(AcpRegistryError::Invalid(format!(
                    "invalid or duplicate agent id `{}`",
                    agent.id
                )));
            }
            if agent.name.trim().is_empty()
                || agent.version.trim().is_empty()
                || agent.version.len() > 128
            {
                return Err(AcpRegistryError::Invalid(format!(
                    "agent `{}` has invalid identity metadata",
                    agent.id
                )));
            }
            let mut offered = 0usize;
            for package in [&agent.distribution.npx, &agent.distribution.uvx]
                .into_iter()
                .flatten()
            {
                offered += 1;
                validate_package(package, &agent.id)?;
            }
            for (platform, binary) in &agent.distribution.binary {
                offered += 1;
                if !valid_platform(platform) {
                    return Err(AcpRegistryError::Invalid(format!(
                        "agent `{}` has invalid platform `{platform}`",
                        agent.id
                    )));
                }
                validate_binary(binary, &agent.id)?;
            }
            if offered == 0 {
                return Err(AcpRegistryError::Invalid(format!(
                    "agent `{}` has no distribution",
                    agent.id
                )));
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&AcpRegistryAgent> {
        let (id, _) = split_agent_coordinate(id);
        self.agents.iter().find(|agent| agent.id == id)
    }
}

/// Resolve common product names to the official registry id. The registry
/// remains authoritative; aliases only make the human-facing CLI forgiving.
#[must_use]
pub fn canonical_agent_id(id: &str) -> String {
    let normalized = id.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "claude" | "claude-code" | "claude-agent" => "claude-acp".to_string(),
        "codex" | "openai-codex" => "codex-acp".to_string(),
        "amp" => "amp-acp".to_string(),
        "zcode-acp" | "zcode-acp-server" => "zcode".to_string(),
        "kimi-code" => "kimi-code".to_string(),
        "kimi-cli" => "kimi".to_string(),
        "vibe" | "vibe-chat" | "mistral" => "mistral-vibe".to_string(),
        "gemini-cli" => "gemini".to_string(),
        "antigravity" | "antigravity-cli" | "agy" | "agy-acp" => "antigravity-acp".to_string(),
        _ => normalized,
    }
}

/// The immutable coordinate persisted in a connected model profile. Discovery
/// follows `latest`, but an already-connected executable never silently changes
/// underneath a run after the 24-hour catalogue refresh.
#[must_use]
pub fn agent_coordinate(id: &str, version: &str) -> String {
    format!("{}@{version}", canonical_agent_id(id))
}

/// A connected profile pinned to one of the agent's own models (discovered
/// over the ACP session-config handshake) extends the coordinate additively:
/// `id@version#model`. The `#model` part selects a model *inside* the session
/// (`session/set_config_option`), never a different launch.
#[must_use]
pub fn agent_coordinate_with_model(id: &str, version: &str, model: &str) -> String {
    format!("{}#{model}", agent_coordinate(id, version))
}

/// Extract the canonical registry id from an alias, a pinned `id@version`
/// coordinate, or a model-pinned `id@version#model` coordinate.
#[must_use]
pub fn agent_id_from_coordinate(coordinate: &str) -> String {
    split_agent_coordinate(coordinate).0
}

/// The agent-model id pinned by an `…#model` coordinate, if any. Additive:
/// coordinates written before model pinning existed have no `#` and yield
/// `None`.
#[must_use]
pub fn agent_model_from_coordinate(coordinate: &str) -> Option<&str> {
    coordinate
        .trim()
        .split_once('#')
        .map(|(_, model)| model)
        .filter(|model| !model.is_empty())
}

fn split_agent_coordinate(coordinate: &str) -> (String, Option<String>) {
    let coordinate = coordinate.trim();
    // A pinned agent model (`…#model`) never affects which agent launches:
    // strip it before resolving the id/version.
    let coordinate = coordinate
        .split_once('#')
        .map_or(coordinate, |(agent, _)| agent);
    if let Some((id, version)) = coordinate.rsplit_once('@') {
        if !id.is_empty() && !version.is_empty() {
            return (canonical_agent_id(id), Some(version.to_string()));
        }
    }
    (canonical_agent_id(coordinate), None)
}

/// Persistent registry/install state below `<data_dir>/acp`.
#[derive(Debug, Clone)]
pub struct AcpRegistryStore {
    root: PathBuf,
}

impl AcpRegistryStore {
    #[must_use]
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: data_dir.into().join("acp"),
        }
    }

    #[must_use]
    pub fn cache_path(&self) -> PathBuf {
        self.root.join("registry.json")
    }

    /// Load the last validated registry without network access.
    pub fn load_cached(&self) -> Result<AcpRegistry, AcpRegistryError> {
        let path = self.cache_path();
        let bytes = std::fs::read(&path).map_err(|source| AcpRegistryError::Io {
            path: path.clone(),
            source,
        })?;
        AcpRegistry::parse(&bytes)
    }

    /// Fetch, validate, and atomically cache the official registry.
    pub async fn refresh(&self) -> Result<AcpRegistry, AcpRegistryError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|error| AcpRegistryError::Fetch(error.to_string()))?;
        let response = client
            .get(ACP_REGISTRY_URL)
            .send()
            .await
            .map_err(|error| AcpRegistryError::Fetch(error.to_string()))?;
        if response.url().scheme() != "https" {
            return Err(AcpRegistryError::Fetch(
                "registry redirect left HTTPS".to_string(),
            ));
        }
        let response = response
            .error_for_status()
            .map_err(|error| AcpRegistryError::Fetch(error.to_string()))?;
        let bytes = bounded_response(response, MAX_REGISTRY_BYTES).await?;
        let registry = AcpRegistry::parse(&bytes)?;
        atomic_write(&self.cache_path(), &bytes)?;
        Ok(registry)
    }

    /// Load a fresh-enough cache, or refresh it automatically. If the network
    /// is unavailable, a previously validated stale cache remains usable so an
    /// offline TUI can still launch already-known agents.
    pub async fn load_or_refresh(&self) -> Result<AcpRegistry, AcpRegistryError> {
        let cached = self.load_cached();
        let fresh = std::fs::metadata(self.cache_path())
            .and_then(|metadata| metadata.modified())
            .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
            .is_ok_and(|age| age <= REGISTRY_MAX_AGE);
        if fresh {
            if let Ok(registry) = &cached {
                return Ok(registry.clone());
            }
        }
        match self.refresh().await {
            Ok(registry) => Ok(registry),
            Err(refresh_error) => cached.or(Err(refresh_error)),
        }
    }

    /// Resolve a cached agent to an executable launch. NPM/Python entries remain
    /// version-pinned commands; binary entries must have been explicitly installed.
    pub fn launch_spec(&self, coordinate: &str) -> Result<AcpLaunchSpec, AcpRegistryError> {
        match resolve_local_acp_agent(coordinate) {
            Ok(Some(spec)) => return Ok(spec),
            Ok(None) => {}
            Err(error) => return Err(error),
        }
        if let Some(spec) = self.installed_binary_specs().into_iter().find(|spec| {
            let (id, version) = split_agent_coordinate(coordinate);
            spec.registry_id == id
                && version
                    .as_deref()
                    .is_none_or(|version| version == spec.version)
        }) {
            return Ok(spec);
        }
        let resolved = self
            .resolve_agent(coordinate)
            .and_then(|agent| self.launch_spec_for(&agent));
        if resolved.is_err() && split_agent_coordinate(coordinate).1.is_none() {
            let id = split_agent_coordinate(coordinate).0;
            if let Some(spec) = self
                .installed_binary_specs()
                .into_iter()
                .find(|spec| spec.registry_id == id)
            {
                return Ok(spec);
            }
        }
        resolved
    }

    /// Reuse installed adapters without downloading or starting an agent.
    #[must_use]
    pub fn installed_agent_specs(&self) -> Vec<AcpLaunchSpec> {
        let mut specs: BTreeMap<_, _> = local_acp_agent_specs()
            .into_iter()
            .map(|spec| (spec.registry_id.clone(), spec))
            .collect();
        for spec in self.installed_binary_specs() {
            specs.entry(spec.registry_id.clone()).or_insert(spec);
        }
        specs.into_values().collect()
    }

    /// The installed Google CLI can use its official ACP companion on first use.
    #[must_use]
    pub fn can_prepare_launch(&self, coordinate: &str) -> bool {
        split_agent_coordinate(coordinate).0 == "antigravity-acp" && resolve_tool("agy").is_some()
    }

    pub async fn prepare_launch(
        &self,
        coordinate: &str,
    ) -> Result<AcpLaunchSpec, AcpRegistryError> {
        match self.launch_spec(coordinate) {
            Ok(spec) => Ok(spec),
            Err(_) if self.can_prepare_launch(coordinate) => {
                self.load_or_refresh().await?;
                self.install(coordinate, false).await
            }
            Err(error) => Err(error),
        }
    }

    fn installed_binary_specs(&self) -> Vec<AcpLaunchSpec> {
        let mut candidates = self
            .load_cached()
            .map(|registry| registry.agents)
            .unwrap_or_default();
        let mut snapshots = Vec::new();
        if let Ok(agents) = std::fs::read_dir(self.root.join("agents")) {
            for agent in agents.take(1000).flatten() {
                if let Ok(versions) = std::fs::read_dir(agent.path()) {
                    snapshots.extend(
                        versions
                            .take(64)
                            .flatten()
                            .map(|version| version.path().join(".registry-agent.json")),
                    );
                }
            }
        }
        snapshots.sort_by_key(|path| {
            std::cmp::Reverse(
                std::fs::metadata(path)
                    .and_then(|metadata| metadata.modified())
                    .ok(),
            )
        });
        for path in snapshots {
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if metadata.len() > 64 * 1024 {
                continue;
            }
            let Some(agent) = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<AcpRegistryAgent>(&bytes).ok())
            else {
                continue;
            };
            if self
                .agent_dir(&agent.id, &agent.version)
                .join(".registry-agent.json")
                != path
            {
                continue;
            }
            if (AcpRegistry {
                version: "snapshot".into(),
                agents: vec![agent.clone()],
            })
            .validate()
            .is_ok()
            {
                candidates.push(agent);
            }
        }
        let mut specs = BTreeMap::new();
        for agent in candidates {
            if agent.distribution.binary.is_empty()
                || specs.contains_key(&agent.id)
                || agent.id == "antigravity-community"
                || agent.repository == "https://github.com/shubzkothekar/antigravity-acp"
            {
                continue;
            }
            if agent.id == "antigravity-acp"
                && !agent
                    .distribution
                    .binary
                    .get(current_platform())
                    .is_some_and(|binary| official_google_binary(&agent.id, binary))
            {
                continue;
            }
            if let Ok(spec) = self.launch_spec_for(&agent) {
                specs.insert(agent.id, spec);
            }
        }
        specs.into_values().collect()
    }

    fn launch_spec_for(&self, agent: &AcpRegistryAgent) -> Result<AcpLaunchSpec, AcpRegistryError> {
        if let Some(package) = &agent.distribution.npx {
            let mut args = vec!["-y".to_string(), package.package.clone()];
            args.extend(package.args.clone());
            let command = resolve_tool("npx").ok_or_else(|| AcpRegistryError::ToolUnavailable {
                tool: "npx".to_string(),
            })?;
            return Ok(spec(agent, command, args, package.env.clone()));
        }
        if let Some(package) = &agent.distribution.uvx {
            let mut args = vec![package.package.clone()];
            args.extend(package.args.clone());
            let command = resolve_tool("uvx").ok_or_else(|| AcpRegistryError::ToolUnavailable {
                tool: "uvx".to_string(),
            })?;
            return Ok(spec(agent, command, args, package.env.clone()));
        }
        let platform = current_platform();
        let binary = agent.distribution.binary.get(platform).ok_or_else(|| {
            AcpRegistryError::UnsupportedPlatform {
                agent: agent.id.clone(),
                platform: platform.to_string(),
            }
        })?;
        let install = self.install_dir(agent);
        let command = safe_join(&install, &binary.cmd)?;
        let marker = std::fs::read_to_string(install.join(".archive.sha256")).ok();
        let marker_matches = marker.as_deref().is_some_and(|digest| {
            digest.len() == 64
                && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                && binary
                    .sha256
                    .as_deref()
                    .is_none_or(|expected| digest.eq_ignore_ascii_case(expected))
        });
        if !command.is_file() || !marker_matches {
            return Err(AcpRegistryError::NotInstalled {
                agent: agent.id.clone(),
            });
        }
        Ok(spec(
            agent,
            command,
            binary.args.clone(),
            binary.env.clone(),
        ))
    }

    /// Explicitly install the current platform's binary distribution. Package
    /// manager distributions need no separate install and simply return their
    /// pinned launch spec.
    pub async fn install(
        &self,
        coordinate: &str,
        allow_unverified: bool,
    ) -> Result<AcpLaunchSpec, AcpRegistryError> {
        match resolve_local_acp_agent(coordinate) {
            Ok(Some(spec)) => return Ok(spec),
            Ok(None) => {}
            Err(error) => return Err(error),
        }
        let agent = self.resolve_agent(coordinate)?;
        if agent.distribution.npx.is_some() || agent.distribution.uvx.is_some() {
            let spec = self.launch_spec_for(&agent)?;
            self.cache_agent_snapshot(&agent)?;
            return Ok(spec);
        }
        // Binary versions are immutable registry coordinates. Reuse a complete
        // prior installation rather than downloading it on every `connect`.
        if let Ok(spec) = self.launch_spec_for(&agent) {
            self.cache_agent_snapshot(&agent)?;
            return Ok(spec);
        }
        let platform = current_platform();
        let binary = agent.distribution.binary.get(platform).ok_or_else(|| {
            AcpRegistryError::UnsupportedPlatform {
                agent: agent.id.clone(),
                platform: platform.to_string(),
            }
        })?;
        let official_google = official_google_binary(&agent.id, binary);
        if binary.sha256.is_none() && !allow_unverified && !official_google {
            return Err(AcpRegistryError::MissingChecksum {
                agent: agent.id.clone(),
            });
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|error| AcpRegistryError::Fetch(error.to_string()))?;
        let response = client
            .get(&binary.archive)
            .send()
            .await
            .map_err(|error| AcpRegistryError::Fetch(error.to_string()))?;
        if response.url().scheme() != "https" {
            return Err(AcpRegistryError::Fetch(
                "binary redirect left HTTPS".to_string(),
            ));
        }
        if official_google && response.url().host_str() != Some("dl.google.com") {
            return Err(AcpRegistryError::Fetch(
                "Google ACP download redirected away from its publisher".into(),
            ));
        }
        let response = response
            .error_for_status()
            .map_err(|error| AcpRegistryError::Fetch(error.to_string()))?;
        let archive = bounded_response(response, MAX_ARCHIVE_BYTES).await?;
        let actual = hex::encode(Sha256::digest(&archive));
        if let Some(expected) = &binary.sha256 {
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(AcpRegistryError::ChecksumMismatch {
                    expected: expected.clone(),
                    actual,
                });
            }
        }
        let target = self.install_dir(&agent);
        let archive_url = binary.archive.clone();
        let archive_command = binary.cmd.clone();
        let archive_digest = actual;
        tokio::task::spawn_blocking(move || {
            install_archive(
                &target,
                &archive_url,
                &archive_command,
                &archive_digest,
                &archive,
            )
        })
        .await
        .map_err(|error| AcpRegistryError::Archive(error.to_string()))??;
        self.cache_agent_snapshot(&agent)?;
        self.launch_spec_for(&agent)
    }

    fn install_dir(&self, agent: &AcpRegistryAgent) -> PathBuf {
        self.agent_dir(&agent.id, &agent.version)
    }

    /// The one place an `id`/`version` pair becomes an on-disk location. Both
    /// the install (`install_dir`) and the lookup (`resolve_agent`) go through
    /// it, because they used to disagree: the install sanitized the components
    /// and the lookup joined them raw, so an agent whose version contained a
    /// separator installed under one path and was then looked for under another
    /// — reporting `NotInstalled` immediately after a successful install.
    fn agent_dir(&self, id: &str, version: &str) -> PathBuf {
        self.root
            .join("agents")
            .join(safe_path_component(id))
            .join(safe_path_component(version))
    }

    fn resolve_agent(&self, coordinate: &str) -> Result<AcpRegistryAgent, AcpRegistryError> {
        let (id, version) = split_agent_coordinate(coordinate);
        if let Some(version) = version {
            // Same normalization the install used (`agent_dir`) — never the raw
            // coordinate. The identity check below still compares the RAW id and
            // version against the snapshot, so sanitizing the path cannot let a
            // coordinate resolve to a different agent than it names.
            let path = self.agent_dir(&id, &version).join(".registry-agent.json");
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                    return Err(AcpRegistryError::NotInstalled {
                        agent: coordinate.to_string(),
                    });
                }
                Err(source) => {
                    return Err(AcpRegistryError::Io {
                        path: path.clone(),
                        source,
                    });
                }
            };
            let agent: AcpRegistryAgent = serde_json::from_slice(&bytes)
                .map_err(|error| AcpRegistryError::Invalid(error.to_string()))?;
            let snapshot = AcpRegistry {
                version: "snapshot".to_string(),
                agents: vec![agent.clone()],
            };
            snapshot.validate()?;
            if agent.id != id || agent.version != version {
                return Err(AcpRegistryError::Invalid(
                    "pinned ACP snapshot does not match its coordinate".to_string(),
                ));
            }
            return Ok(agent);
        }
        if let Some(agent) = community_acp_agent(&id) {
            return Ok(agent);
        }
        let registry = self.load_cached()?;
        registry
            .get(&id)
            .cloned()
            .ok_or(AcpRegistryError::UnknownAgent(id))
    }

    fn cache_agent_snapshot(&self, agent: &AcpRegistryAgent) -> Result<(), AcpRegistryError> {
        let bytes = serde_json::to_vec_pretty(agent)
            .map_err(|error| AcpRegistryError::Invalid(error.to_string()))?;
        atomic_write(
            &self.install_dir(agent).join(".registry-agent.json"),
            &bytes,
        )
    }
}

/// The legacy community bridge has a separate identity from Google's ACP server.
#[must_use]
pub fn community_acp_agent(id: &str) -> Option<AcpRegistryAgent> {
    if canonical_agent_id(id) != "antigravity-community" {
        return None;
    }
    let mut binary = BTreeMap::new();
    for (platform, asset, sha256) in [
        (
            "darwin-aarch64",
            "agy-acp-darwin-arm64",
            "7936bd5fd662e6514755a8e8b19aba88b2f01b94d082d93bbc238cb4bdc9c2e8",
        ),
        (
            "darwin-x86_64",
            "agy-acp-darwin-x64",
            "4265454974b67142061539270fb6401229034098590762b2b0c30be68ff5ebdc",
        ),
        (
            "linux-aarch64",
            "agy-acp-linux-arm64",
            "7eec158411e1939c6ad6298b52ee2691425b666a448ee12c07ccf59b55067652",
        ),
        (
            "linux-x86_64",
            "agy-acp-linux-x64",
            "ed900c0ebb72ff505ec5c64296b534472927140514aacad607af645320e6a3d1",
        ),
    ] {
        binary.insert(
            platform.to_string(),
            AcpBinaryDistribution {
                archive: format!(
                    "https://github.com/shubzkothekar/antigravity-acp/releases/download/v1.0.0/{asset}"
                ),
                cmd: format!("./{asset}"),
                sha256: Some(sha256.to_string()),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
        );
    }
    Some(AcpRegistryAgent {
        id: "antigravity-community".to_string(),
        name: "Google Antigravity (community ACP bridge)".to_string(),
        version: "1.0.0".to_string(),
        description: "Third-party Antigravity ACP bridge; not provided or endorsed by Google"
            .to_string(),
        repository: "https://github.com/shubzkothekar/antigravity-acp".to_string(),
        distribution: AcpDistribution {
            npx: None,
            uvx: None,
            binary,
        },
    })
}

/// Discover Kimi Code's native ACP server when its own installer placed the
/// executable in `~/.kimi-code/bin` (or it is the `kimi` resolved on PATH).
/// Unlike the official registry's older `kimi`/Kimi CLI entry, this shares the
/// `kimi login` credentials and `kimi acp` implementation the user actually
/// installed. A pinned local coordinate refuses a later version until the user
/// reconnects, preserving the same no-silent-upgrade behavior as registry
/// packages.
#[must_use]
pub fn local_kimi_code_spec() -> Option<AcpLaunchSpec> {
    let home_candidate = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".kimi-code").join("bin").join("kimi"));
    let command = home_candidate
        .filter(|candidate| candidate.is_file())
        .or_else(|| resolve_tool("kimi"))?;
    let metadata = command
        .parent()?
        .parent()?
        .join("updates")
        .join("install.json");
    let version = local_tool_version(&command)
        .or_else(|| {
            std::fs::read(&metadata)
                .ok()
                .filter(|bytes| bytes.len() <= 64 * 1024)
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|value| {
                    value
                        .get("lastSuccess")?
                        .get("version")?
                        .as_str()
                        .map(ToOwned::to_owned)
                })
        })
        .filter(|version| {
            !version.is_empty()
                && version.len() <= 128
                && version
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
        })
        .unwrap_or_else(|| "local".to_string());
    Some(AcpLaunchSpec {
        registry_id: "kimi-code".to_string(),
        name: "Kimi Code".to_string(),
        version,
        command,
        args: vec!["acp".to_string()],
        env: BTreeMap::new(),
    })
}

/// Discover ACP servers supplied by locally installed tools without downloads.
#[must_use]
pub fn local_acp_agent_spec(id: &str) -> Option<AcpLaunchSpec> {
    let id = canonical_agent_id(id);
    match id.as_str() {
        "claude-acp" => local_binary_spec(
            "claude-acp",
            "Claude Code",
            &["claude-agent-acp", "claude-code-acp"],
            &[],
        ),
        "codex-acp" => local_binary_spec("codex-acp", "Codex", &["codex-acp"], &[]),
        "amp-acp" => local_binary_spec("amp-acp", "Amp", &["amp-acp"], &[]),
        "zcode" => local_zcode_spec(),
        "gemini" => local_binary_spec("gemini", "Gemini CLI", &["gemini"], &["--acp"]),
        "opencode" => local_binary_spec("opencode", "OpenCode", &["opencode"], &["acp"]),
        "copilot" => local_binary_spec("copilot", "GitHub Copilot", &["copilot"], &["--acp"]),
        "cline" => local_binary_spec("cline", "Cline", &["cline"], &["--acp"]),
        "kimi-code" => local_kimi_code_spec(),
        "junie" => local_binary_spec("junie", "Junie", &["junie"], &["--acp=true"]),
        "cursor" => local_binary_spec("cursor", "Cursor", &["cursor-agent"], &["acp"]),
        "cortex-code" => {
            local_binary_spec("cortex-code", "Cortex Code", &["cortex"], &["acp", "serve"])
        }
        "corust-agent" => {
            local_binary_spec("corust-agent", "Corust Agent", &["corust-agent-acp"], &[])
        }
        "crow-cli" => local_binary_spec("crow-cli", "Crow CLI", &["crow-cli"], &["acp"]),
        "devin" => local_binary_spec("devin", "Devin", &["devin"], &["acp"]),
        "stakpak" => local_binary_spec("stakpak", "Stakpak", &["stakpak"], &["acp"]),
        "antigravity-acp" => local_binary_spec(
            "antigravity-acp",
            "Google Antigravity",
            &["agy_acp_server.par", "agy_acp_server.exe", "agy_acp_server"],
            if cfg!(target_os = "linux") {
                &["--uid="]
            } else {
                &[]
            },
        ),
        "antigravity-community" => local_binary_spec(
            "antigravity-community",
            "Google Antigravity (community ACP bridge)",
            &[
                "agy-acp",
                "antigravity-acp",
                "agy-acp-darwin-arm64",
                "agy-acp-darwin-x64",
                "agy-acp-linux-arm64",
                "agy-acp-linux-x64",
            ],
            &[],
        ),
        _ => None,
    }
}

/// Every installed local ACP adapter Codypendent can launch without weakening
/// the official registry's checksum policy.
#[must_use]
pub fn local_acp_agent_specs() -> Vec<AcpLaunchSpec> {
    [
        "claude-acp",
        "codex-acp",
        "amp-acp",
        "zcode",
        "gemini",
        "opencode",
        "copilot",
        "cline",
        "kimi-code",
        "junie",
        "cursor",
        "cortex-code",
        "corust-agent",
        "crow-cli",
        "devin",
        "stakpak",
        "antigravity-acp",
    ]
    .into_iter()
    .filter_map(local_acp_agent_spec)
    .collect()
}

/// Agent integrations shown even when their runtime is not installed yet.
#[must_use]
pub fn supported_agent_names() -> &'static [(&'static str, &'static str)] {
    &[
        ("codex-acp", "Codex"),
        ("claude-acp", "Claude Code"),
        ("gemini", "Gemini CLI"),
        ("antigravity-acp", "Google Antigravity"),
        ("copilot", "GitHub Copilot"),
        ("opencode", "OpenCode"),
        ("cline", "Cline"),
        ("amp-acp", "Amp"),
        ("zcode", "ZCode"),
        ("kimi-code", "Kimi Code"),
    ]
}

fn local_binary_spec(
    registry_id: &str,
    name: &str,
    commands: &[&str],
    args: &[&str],
) -> Option<AcpLaunchSpec> {
    let mut command = commands
        .iter()
        .find_map(|command| resolve_tool(command))
        .or_else(|| cached_adapter(commands))?;
    if registry_id == "cline" && local_tool_version(&command).is_none() {
        command = cached_adapter(commands)
            .filter(|candidate| candidate != &command)
            .filter(|candidate| local_tool_version(candidate).is_some())?;
    }
    let version = installed_package_version(&command)
        .or_else(|| local_tool_version(&command))
        .unwrap_or_else(|| "local".to_string());
    let mut env = BTreeMap::new();
    if let Some(node) = resolve_tool("node") {
        let mut directories: Vec<_> = node.parent().map(Path::to_path_buf).into_iter().collect();
        directories.extend(
            std::env::var_os("PATH")
                .into_iter()
                .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>()),
        );
        if let Ok(path) = std::env::join_paths(directories) {
            env.insert("PATH".to_string(), path.to_string_lossy().into_owned());
        }
    }
    prefer_installed_vendor_cli(registry_id, &mut env);
    antigravity_companion(registry_id, &command, &mut env);
    Some(AcpLaunchSpec {
        registry_id: registry_id.to_string(),
        name: name.to_string(),
        version,
        command,
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        env,
    })
}

fn local_zcode_spec() -> Option<AcpLaunchSpec> {
    let command = resolve_tool("node")?;
    let configured = std::env::var_os("ZCODE_BIN").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut candidates: Vec<_> = configured.into_iter().collect();
    candidates.extend(resolve_tool("zcode"));
    if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from(
            "/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs",
        ));
        if let Some(home) = &home {
            candidates.push(home.join("Applications/ZCode.app/Contents/Resources/glm/zcode.cjs"));
        }
    } else if cfg!(target_os = "windows") {
        for root in ["LOCALAPPDATA", "ProgramFiles"] {
            if let Some(root) = std::env::var_os(root) {
                let root = PathBuf::from(root);
                if root.ends_with("Local") {
                    candidates.push(root.join("Programs/ZCode/resources/glm/zcode.cjs"));
                } else {
                    candidates.push(root.join("ZCode/resources/glm/zcode.cjs"));
                }
            }
        }
    } else {
        candidates.push(PathBuf::from("/opt/ZCode/resources/glm/zcode.cjs"));
        if let Some(home) = &home {
            candidates.push(home.join(".local/share/ZCode/resources/glm/zcode.cjs"));
        }
    }
    let runtime = candidates.into_iter().find(|path| path.is_file())?;
    let mut env = BTreeMap::new();
    env.insert(
        "CODYPENDENT_ZCODE_PATH".into(),
        runtime.to_string_lossy().into_owned(),
    );
    if let Some(resources) = runtime.parent().and_then(Path::parent) {
        let bundled = resources.join("config/provider/zcode-builtin.json");
        if bundled.is_file() {
            if std::env::var_os("ZCODE_BUILTIN_PROVIDER_CONFIG_FILE").is_none() {
                env.insert(
                    "ZCODE_BUILTIN_PROVIDER_CONFIG_FILE".into(),
                    bundled.to_string_lossy().into_owned(),
                );
            }
            let data_base = std::env::var_os("ZCODE_DATA_BASE_DIR")
                .map(PathBuf::from)
                .or(home);
            if std::env::var_os("ZCODE_PERSONAL_PROVIDER_CONFIG_FILE").is_none() {
                if let Some(base) = data_base {
                    env.insert(
                        "ZCODE_PERSONAL_PROVIDER_CONFIG_FILE".into(),
                        base.join(".zcode/v2/provider_config.json")
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
    }
    Some(AcpLaunchSpec {
        registry_id: "zcode".into(),
        name: "ZCode".into(),
        version: "local".into(),
        command,
        args: vec![
            "--input-type=module".into(),
            "-e".into(),
            include_str!("zcode_acp.mjs").into(),
        ],
        env,
    })
}

fn cached_adapter(commands: &[&str]) -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("HOME")?).join(".npm/_npx");
    let mut candidates: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .take(256)
        .flatten()
        .flat_map(|entry| {
            commands
                .iter()
                .map(move |command| entry.path().join("node_modules/.bin").join(command))
        })
        .filter(|path| path.is_file())
        .collect();
    candidates.sort_by_key(|path| {
        std::cmp::Reverse(
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok(),
        )
    });
    candidates.into_iter().next()
}

fn installed_package_version(command: &Path) -> Option<String> {
    let command = command.canonicalize().ok()?;
    command.ancestors().skip(1).take(6).find_map(|directory| {
        let path = directory.join("package.json");
        if std::fs::metadata(&path).ok()?.len() > 64 * 1024 {
            return None;
        }
        let package: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        package
            .get("version")?
            .as_str()
            .filter(|version| {
                !version.is_empty()
                    && version.len() <= 128
                    && version.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+')
                    })
            })
            .map(ToOwned::to_owned)
    })
}

fn local_tool_version(command: &Path) -> Option<String> {
    use std::process::Stdio;
    use std::time::Instant;

    let mut child = std::process::Command::new(command)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let output = child.wait_with_output().ok()?;
                let output = std::str::from_utf8(&output.stdout).ok()?.trim();
                return output
                    .split_ascii_whitespace()
                    .map(|part| {
                        part.trim_matches(|ch: char| {
                            !ch.is_ascii_alphanumeric() && !matches!(ch, '.' | '-' | '+')
                        })
                    })
                    .find(|part| {
                        !part.is_empty()
                            && part.len() <= 128
                            && part.bytes().any(|byte| byte.is_ascii_digit())
                            && part.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+')
                            })
                    })
                    .map(ToOwned::to_owned);
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn resolve_local_acp_agent(coordinate: &str) -> Result<Option<AcpLaunchSpec>, AcpRegistryError> {
    let (id, pinned) = split_agent_coordinate(coordinate);
    let optional = matches!(
        id.as_str(),
        "claude-acp"
            | "codex-acp"
            | "amp-acp"
            | "gemini"
            | "opencode"
            | "copilot"
            | "cline"
            | "antigravity-acp"
            | "antigravity-community"
    );
    if !matches!(
        id.as_str(),
        "claude-acp"
            | "codex-acp"
            | "amp-acp"
            | "zcode"
            | "gemini"
            | "opencode"
            | "copilot"
            | "cline"
            | "kimi-code"
            | "junie"
            | "cursor"
            | "cortex-code"
            | "corust-agent"
            | "crow-cli"
            | "devin"
            | "stakpak"
            | "antigravity-acp"
            | "antigravity-community"
    ) {
        return Ok(None);
    }
    let local = local_acp_agent_spec(&id);
    if optional
        && local.as_ref().is_none_or(|spec| {
            pinned
                .as_deref()
                .is_some_and(|version| version != spec.version)
        })
    {
        return Ok(None);
    }
    let spec = local.ok_or_else(|| AcpRegistryError::ToolUnavailable {
        tool: match id.as_str() {
            "claude-acp" => "claude-agent-acp",
            "codex-acp" => "codex-acp",
            "amp-acp" => "amp-acp",
            "zcode" => "ZCode",
            "gemini" => "gemini",
            "opencode" => "opencode",
            "copilot" => "copilot",
            "cline" => "cline",
            "kimi-code" => "kimi",
            "junie" => "junie",
            "cursor" => "cursor-agent",
            "cortex-code" => "cortex",
            "corust-agent" => "corust-agent-acp",
            "crow-cli" => "crow-cli",
            "devin" => "devin",
            "stakpak" => "stakpak",
            "antigravity-acp" => "agy_acp_server.par",
            "antigravity-community" => "agy-acp",
            _ => unreachable!("known local ACP id"),
        }
        .into(),
    })?;
    if pinned
        .as_deref()
        .is_some_and(|version| version != spec.version)
    {
        return Err(AcpRegistryError::NotInstalled {
            agent: coordinate.to_string(),
        });
    }
    Ok(Some(spec))
}

fn spec(
    agent: &AcpRegistryAgent,
    command: PathBuf,
    args: Vec<String>,
    mut env: BTreeMap<String, String>,
) -> AcpLaunchSpec {
    prefer_installed_vendor_cli(&agent.id, &mut env);
    antigravity_companion(&agent.id, &command, &mut env);
    AcpLaunchSpec {
        registry_id: agent.id.clone(),
        name: agent.name.clone(),
        version: agent.version.clone(),
        command,
        args,
        env,
    }
}

fn antigravity_companion(id: &str, command: &Path, env: &mut BTreeMap<String, String>) {
    if id != "antigravity-acp"
        || env.contains_key("ANTIGRAVITY_HARNESS_PATH")
        || std::env::var_os("ANTIGRAVITY_HARNESS_PATH").is_some()
    {
        return;
    }
    let Some(parent) = command.parent() else {
        return;
    };
    let companion = parent.join(if cfg!(windows) {
        "localharness_external.exe"
    } else {
        "localharness_external"
    });
    if companion.is_file() {
        env.insert(
            "ANTIGRAVITY_HARNESS_PATH".into(),
            companion.to_string_lossy().into_owned(),
        );
    }
}

fn prefer_installed_vendor_cli(registry_id: &str, env: &mut BTreeMap<String, String>) {
    let (variable, executable) = match registry_id {
        "codex-acp" => ("CODEX_PATH", "codex"),
        "claude-acp" => ("CLAUDE_CODE_EXECUTABLE", "claude"),
        "amp-acp" => ("AMP_CLI_PATH", "amp"),
        _ => return,
    };
    if env.contains_key(variable) || std::env::var_os(variable).is_some_and(|path| !path.is_empty())
    {
        return;
    }
    if let Some(command) = resolve_tool(executable) {
        env.insert(variable.to_string(), command.to_string_lossy().into_owned());
    }
}

fn official_google_binary(id: &str, binary: &AcpBinaryDistribution) -> bool {
    if id != "antigravity-acp"
        || !binary.env.is_empty()
        || binary.args.iter().any(|arg| arg != "--uid=")
        || !matches!(
            binary.cmd.as_str(),
            "./agy_acp_server.par" | "./agy_acp_server.exe"
        )
    {
        return false;
    }
    reqwest::Url::parse(&binary.archive).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("dl.google.com")
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path().starts_with("/agy-extensions/releases/")
            && url.path().ends_with(".zip")
    })
}

async fn bounded_response(
    response: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, AcpRegistryError> {
    if response
        .content_length()
        .is_some_and(|length| length > cap as u64)
    {
        return Err(if cap == MAX_REGISTRY_BYTES {
            AcpRegistryError::RegistryTooLarge
        } else {
            AcpRegistryError::ArchiveTooLarge
        });
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| AcpRegistryError::Fetch(error.to_string()))?;
        if bytes.len().saturating_add(chunk.len()) > cap {
            return Err(if cap == MAX_REGISTRY_BYTES {
                AcpRegistryError::RegistryTooLarge
            } else {
                AcpRegistryError::ArchiveTooLarge
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn install_archive(
    target: &Path,
    url: &str,
    command: &str,
    archive_digest: &str,
    bytes: &[u8],
) -> Result<(), AcpRegistryError> {
    let parent = target
        .parent()
        .ok_or_else(|| AcpRegistryError::Archive("installation path has no parent".to_string()))?;
    std::fs::create_dir_all(parent).map_err(|source| AcpRegistryError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let stage = parent.join(format!(".install-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir(&stage).map_err(|source| AcpRegistryError::Io {
        path: stage.clone(),
        source,
    })?;
    let archive_path = reqwest::Url::parse(url)
        .map_err(|error| AcpRegistryError::Archive(format!("invalid archive URL: {error}")))?
        .path()
        .to_ascii_lowercase();
    let result = if archive_path.ends_with(".zip") {
        extract_zip(bytes, &stage)
    } else if archive_path.ends_with(".tar.gz") || archive_path.ends_with(".tgz") {
        extract_tar_gz(bytes, &stage)
    } else if archive_path.ends_with(".tar.bz2") {
        extract_tar_bz2(bytes, &stage)
    } else {
        install_raw_binary(bytes, &stage, command)
    };
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(error);
    }
    let staged_command = safe_join(&stage, command)?;
    if !staged_command.is_file() {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(AcpRegistryError::Archive(format!(
            "archive does not contain declared command `{command}`"
        )));
    }
    set_executable(&staged_command)?;
    std::fs::write(stage.join(".archive.sha256"), archive_digest).map_err(|source| {
        AcpRegistryError::Io {
            path: stage.join(".archive.sha256"),
            source,
        }
    })?;
    if target.exists() {
        std::fs::remove_dir_all(target).map_err(|source| AcpRegistryError::Io {
            path: target.to_path_buf(),
            source,
        })?;
    }
    std::fs::rename(&stage, target).map_err(|source| AcpRegistryError::Io {
        path: target.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn extract_tar_gz(bytes: &[u8], target: &Path) -> Result<(), AcpRegistryError> {
    let decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
    extract_tar(decoder, target)
}

fn extract_tar_bz2(bytes: &[u8], target: &Path) -> Result<(), AcpRegistryError> {
    let decoder = bzip2::read::BzDecoder::new(Cursor::new(bytes));
    extract_tar(decoder, target)
}

fn extract_tar(reader: impl std::io::Read, target: &Path) -> Result<(), AcpRegistryError> {
    #[derive(Debug)]
    enum DeferredLinkKind {
        Symbolic,
        Hard,
    }
    #[derive(Debug)]
    struct DeferredLink {
        path: PathBuf,
        target: PathBuf,
        normalized_target: PathBuf,
        kind: DeferredLinkKind,
    }

    let mut archive = tar::Archive::new(reader);
    let mut seen = BTreeSet::new();
    let mut links = Vec::new();
    let mut total = 0u64;
    for (index, entry) in archive
        .entries()
        .map_err(|error| AcpRegistryError::Archive(error.to_string()))?
        .enumerate()
    {
        if index >= MAX_ARCHIVE_ENTRIES {
            return Err(AcpRegistryError::Archive(
                "too many archive entries".to_string(),
            ));
        }
        let mut entry = entry.map_err(|error| AcpRegistryError::Archive(error.to_string()))?;
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() && !kind.is_symlink() && !kind.is_hard_link() {
            return Err(AcpRegistryError::Archive(
                "special archive entries are forbidden".to_string(),
            ));
        }
        let raw_relative = entry
            .path()
            .map_err(|error| AcpRegistryError::Archive(error.to_string()))?
            .into_owned();
        let relative = clean_relative_path(&raw_relative)?;
        if !seen.insert(relative.clone()) {
            return Err(AcpRegistryError::Archive(
                "duplicate archive path".to_string(),
            ));
        }
        if kind.is_symlink() || kind.is_hard_link() {
            let link_target = entry
                .link_name()
                .map_err(|error| AcpRegistryError::Archive(error.to_string()))?
                .ok_or_else(|| AcpRegistryError::Archive("link has no target".to_string()))?
                .into_owned();
            let base = if kind.is_symlink() {
                relative.parent().unwrap_or_else(|| Path::new(""))
            } else {
                Path::new("")
            };
            let normalized_target = normalize_link_target(base, &link_target)?;
            links.push(DeferredLink {
                path: relative,
                target: link_target,
                normalized_target,
                kind: if kind.is_symlink() {
                    DeferredLinkKind::Symbolic
                } else {
                    DeferredLinkKind::Hard
                },
            });
            continue;
        }
        if kind.is_file() {
            total = total.saturating_add(entry.size());
            if total > MAX_EXTRACTED_BYTES {
                return Err(AcpRegistryError::Archive(
                    "archive expands beyond the extraction limit".to_string(),
                ));
            }
        }
        let output = target.join(relative);
        if kind.is_file() {
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent).map_err(|source| AcpRegistryError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
        }
        entry
            .unpack(&output)
            .map_err(|error| AcpRegistryError::Archive(error.to_string()))?;
    }

    // Never extract through a link. Links are created only after every regular
    // entry has landed, and an archive may not place one link beneath another.
    for link in &links {
        if links.iter().any(|other| {
            other.path != link.path
                && link.path.starts_with(&other.path)
                && other.path.components().count() < link.path.components().count()
        }) {
            return Err(AcpRegistryError::Archive(
                "nested archive links are forbidden".to_string(),
            ));
        }
    }
    for link in links {
        let output = target.join(&link.path);
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).map_err(|source| AcpRegistryError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        match link.kind {
            DeferredLinkKind::Hard => {
                let source = target.join(&link.normalized_target);
                let size = source
                    .metadata()
                    .map_err(|source_error| AcpRegistryError::Io {
                        path: source.clone(),
                        source: source_error,
                    })?
                    .len();
                total = total.saturating_add(size);
                if total > MAX_EXTRACTED_BYTES {
                    return Err(AcpRegistryError::Archive(
                        "archive expands beyond the extraction limit".to_string(),
                    ));
                }
                std::fs::copy(&source, &output).map_err(|source_error| AcpRegistryError::Io {
                    path: output,
                    source: source_error,
                })?;
            }
            DeferredLinkKind::Symbolic => {
                // A valid bundle may contain a chain such as
                // `Python -> Versions/Current/Python` where `Current` is a
                // second deferred symlink. Every individual target has already
                // been lexically proven to remain beneath the extraction root;
                // existence is intentionally checked by the launched binary,
                // after the complete link set has been materialized.
                #[cfg(unix)]
                std::os::unix::fs::symlink(&link.target, &output).map_err(|source| {
                    AcpRegistryError::Io {
                        path: output,
                        source,
                    }
                })?;
                #[cfg(not(unix))]
                return Err(AcpRegistryError::Archive(
                    "symbolic links are unsupported on this platform".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn install_raw_binary(bytes: &[u8], target: &Path, command: &str) -> Result<(), AcpRegistryError> {
    let output = safe_join(target, command)?;
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|source| AcpRegistryError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&output, bytes).map_err(|source| AcpRegistryError::Io {
        path: output.clone(),
        source,
    })?;
    set_executable(&output)
}

fn extract_zip(bytes: &[u8], target: &Path) -> Result<(), AcpRegistryError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| AcpRegistryError::Archive(error.to_string()))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(AcpRegistryError::Archive(
            "too many archive entries".to_string(),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| AcpRegistryError::Archive(error.to_string()))?;
        let raw_relative = entry
            .enclosed_name()
            .ok_or_else(|| AcpRegistryError::Archive("archive path escapes root".to_string()))?
            .to_path_buf();
        let relative = clean_relative_path(&raw_relative)?;
        if !seen.insert(relative.clone()) {
            return Err(AcpRegistryError::Archive(
                "duplicate archive path".to_string(),
            ));
        }
        if let Some(mode) = entry.unix_mode() {
            let file_type = mode & 0o170000;
            if file_type != 0 && file_type != 0o100000 && file_type != 0o040000 {
                return Err(AcpRegistryError::Archive(
                    "links and special archive entries are forbidden".to_string(),
                ));
            }
        }
        let output = target.join(&relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&output).map_err(|source| AcpRegistryError::Io {
                path: output,
                source,
            })?;
            continue;
        }
        total = total.saturating_add(entry.size());
        if total > MAX_EXTRACTED_BYTES {
            return Err(AcpRegistryError::Archive(
                "archive expands beyond the extraction limit".to_string(),
            ));
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).map_err(|source| AcpRegistryError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut file = std::fs::File::create(&output).map_err(|source| AcpRegistryError::Io {
            path: output.clone(),
            source,
        })?;
        std::io::copy(&mut entry, &mut file)
            .map_err(|error| AcpRegistryError::Archive(error.to_string()))?;
        set_executable_if_declared(&output, entry.unix_mode())?;
    }
    Ok(())
}

fn set_executable_if_declared(path: &Path, mode: Option<u32>) -> Result<(), AcpRegistryError> {
    #[cfg(unix)]
    if mode.is_some_and(|mode| mode & 0o111 != 0) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(
            |source| AcpRegistryError::Io {
                path: path.to_path_buf(),
                source,
            },
        )?;
    }
    Ok(())
}

fn set_executable(path: &Path) -> Result<(), AcpRegistryError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(
            |source| AcpRegistryError::Io {
                path: path.to_path_buf(),
                source,
            },
        )?;
    }
    Ok(())
}

fn validate_package(package: &AcpPackageDistribution, agent: &str) -> Result<(), AcpRegistryError> {
    let coordinate = package.package.as_str();
    let pinned = coordinate
        .rsplit_once('@')
        .is_some_and(|(name, version)| !name.is_empty() && !version.is_empty())
        || coordinate
            .split_once("==")
            .is_some_and(|(name, version)| !name.is_empty() && !version.is_empty());
    if coordinate.trim() != coordinate
        || coordinate.is_empty()
        || coordinate.len() > 512
        || coordinate.starts_with('-')
        || coordinate.chars().any(char::is_whitespace)
        || !pinned
    {
        return Err(AcpRegistryError::Invalid(format!(
            "agent `{agent}` has an invalid or unpinned package coordinate"
        )));
    }
    validate_process_fields(&package.args, &package.env, agent)
}

fn validate_binary(binary: &AcpBinaryDistribution, agent: &str) -> Result<(), AcpRegistryError> {
    let url = reqwest::Url::parse(&binary.archive)
        .map_err(|error| AcpRegistryError::Invalid(format!("agent `{agent}` URL: {error}")))?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(AcpRegistryError::Invalid(format!(
            "agent `{agent}` binary URL must be HTTPS"
        )));
    }
    validate_relative_path(Path::new(&binary.cmd))?;
    if let Some(digest) = &binary.sha256 {
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(AcpRegistryError::Invalid(format!(
                "agent `{agent}` has an invalid SHA-256"
            )));
        }
    }
    validate_process_fields(&binary.args, &binary.env, agent)
}

fn validate_process_fields(
    args: &[String],
    env: &BTreeMap<String, String>,
    agent: &str,
) -> Result<(), AcpRegistryError> {
    if args.len() > MAX_ARGS
        || args
            .iter()
            .any(|arg| arg.len() > 16 * 1024 || arg.contains('\0'))
        || env.len() > MAX_ENV
        || env.iter().any(|(key, value)| {
            key.is_empty()
                || key.len() > 256
                || value.len() > 64 * 1024
                || key.contains(['=', '\0'])
                || value.contains('\0')
        })
    {
        return Err(AcpRegistryError::Invalid(format!(
            "agent `{agent}` has invalid process arguments or environment"
        )));
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), AcpRegistryError> {
    if path.as_os_str().as_encoded_bytes().len() > MAX_PATH_BYTES
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(AcpRegistryError::Archive(
            "archive path escapes root or is too long".to_string(),
        ));
    }
    Ok(())
}

fn clean_relative_path(path: &Path) -> Result<PathBuf, AcpRegistryError> {
    validate_relative_path(path)?;
    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => cleaned.push(part),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(AcpRegistryError::Archive(
                    "archive path escapes root".to_string(),
                ));
            }
        }
    }
    Ok(cleaned)
}

fn normalize_link_target(base: &Path, target: &Path) -> Result<PathBuf, AcpRegistryError> {
    if target.as_os_str().as_encoded_bytes().len() > MAX_PATH_BYTES || target.is_absolute() {
        return Err(AcpRegistryError::Archive(
            "archive link target escapes root or is too long".to_string(),
        ));
    }
    let mut normalized = clean_relative_path(base)?;
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(AcpRegistryError::Archive(
                        "archive link target escapes root".to_string(),
                    ));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(AcpRegistryError::Archive(
                    "archive link target escapes root".to_string(),
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(AcpRegistryError::Archive(
            "archive link target is empty".to_string(),
        ));
    }
    Ok(normalized)
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, AcpRegistryError> {
    let trimmed = relative
        .strip_prefix("./")
        .or_else(|| relative.strip_prefix(".\\"))
        .unwrap_or(relative);
    let relative = clean_relative_path(Path::new(trimmed))?;
    Ok(root.join(relative))
}

/// Flatten one registry `id`/`version` into a single directory name. Registry
/// validation still allows a version containing `/`, `\` or `..`, and a version
/// is not a path — so it is collapsed rather than trusted, keeping every
/// installed agent inside `<root>/agents/<id>/<version>/`.
///
/// Call it through [`AcpRegistryStore::agent_dir`] rather than directly: the
/// install and the lookup must apply exactly the same mapping or an agent
/// installs and then fails to resolve.
fn safe_path_component(value: &str) -> String {
    value.replace(['/', '\\'], "_").replace("..", "_")
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_platform(platform: &str) -> bool {
    matches!(
        platform,
        "darwin-aarch64"
            | "darwin-x86_64"
            | "linux-aarch64"
            | "linux-x86_64"
            | "windows-aarch64"
            | "windows-x86_64"
    )
}

#[must_use]
pub fn current_platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-aarch64",
        ("macos", "x86_64") => "darwin-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("linux", "x86_64") => "linux-x86_64",
        ("windows", "aarch64") => "windows-aarch64",
        ("windows", "x86_64") => "windows-x86_64",
        _ => "unsupported",
    }
}

fn resolve_tool(name: &str) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin) = exe.parent() {
            candidates.push(bin.join("node-runtime").join("bin").join(name));
            if let Some(prefix) = bin.parent() {
                candidates.push(
                    prefix
                        .join("lib")
                        .join("codypendent")
                        .join("node-runtime")
                        .join("bin")
                        .join(name),
                );
            }
        }
    }
    if let Some(path) = candidates.into_iter().find(|candidate| candidate.is_file()) {
        return Some(path);
    }
    let mut directories: Vec<_> = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .collect();
    directories.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        directories.extend([
            home.join(".local/bin"),
            home.join(".cargo/bin"),
            home.join(".bun/bin"),
        ]);
    }
    directories
        .into_iter()
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), AcpRegistryError> {
    let parent = path
        .parent()
        .ok_or_else(|| AcpRegistryError::Invalid("cache path has no parent".to_string()))?;
    std::fs::create_dir_all(parent).map_err(|source| AcpRegistryError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let temp = parent.join(format!(".registry-{}.tmp", uuid::Uuid::now_v7()));
    std::fs::write(&temp, bytes).map_err(|source| AcpRegistryError::Io {
        path: temp.clone(),
        source,
    })?;
    std::fs::rename(&temp, path).map_err(|source| AcpRegistryError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| AcpRegistryError::Io {
                path: path.to_path_buf(),
                source,
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const FIXTURE: &str = r#"{
      "version":"1.0.0",
      "agents":[
        {"id":"codex-acp","name":"Codex","version":"1.2.3","distribution":{"npx":{"package":"@acp/codex@1.2.3","args":["--stdio"]}}},
        {"id":"fast-agent","name":"Fast Agent","version":"2.0.0","distribution":{"uvx":{"package":"fast-agent==2.0.0"}}},
        {"id":"native","name":"Native","version":"3.0.0","distribution":{"binary":{"linux-x86_64":{"archive":"https://example.com/native.tar.gz","cmd":"./bin/native","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}}}
      ]
    }"#;

    #[test]
    fn parses_all_distribution_kinds_and_pins_package_versions() {
        let registry = AcpRegistry::parse(FIXTURE.as_bytes()).expect("valid registry");
        assert_eq!(registry.agents.len(), 3);
        let dir = tempfile::tempdir().expect("tempdir");
        let store = AcpRegistryStore::new(dir.path());
        atomic_write(&store.cache_path(), FIXTURE.as_bytes()).expect("cache fixture");
        store
            .cache_agent_snapshot(registry.get("codex-acp").expect("codex fixture"))
            .expect("pin fixture");
        match store.launch_spec("codex-acp@1.2.3") {
            Ok(codex) => {
                assert_eq!(codex.args, ["-y", "@acp/codex@1.2.3", "--stdio"])
            }
            Err(AcpRegistryError::ToolUnavailable { tool }) => assert_eq!(tool, "npx"),
            Err(error) => panic!("unexpected npx launch error: {error}"),
        }
        match store.launch_spec("fast-agent") {
            Ok(fast) => assert_eq!(fast.args, ["fast-agent==2.0.0"]),
            Err(AcpRegistryError::ToolUnavailable { tool }) => assert_eq!(tool, "uvx"),
            Err(error) => panic!("unexpected uvx launch error: {error}"),
        }
    }

    #[test]
    fn installed_adapter_version_is_read_without_starting_its_process() {
        let directory = tempfile::tempdir().expect("temp dir");
        let package = directory
            .path()
            .join("node_modules/@agentclientprotocol/claude-agent-acp");
        std::fs::create_dir_all(package.join("dist")).unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"@agentclientprotocol/claude-agent-acp","version":"0.75.1"}"#,
        )
        .unwrap();
        let executable = package.join("dist/index.js");
        std::fs::write(&executable, "process.exit(1)").unwrap();
        assert_eq!(
            installed_package_version(&executable).as_deref(),
            Some("0.75.1")
        );
        std::fs::write(package.join("package.json"), r#"{"version":"../invalid"}"#).unwrap();
        assert!(installed_package_version(&executable).is_none());
    }

    #[test]
    fn installed_cline_uses_its_native_acp_mode_before_registry_packages() {
        let Some(local) = local_acp_agent_spec("cline") else {
            return;
        };
        let command = local.command;
        let directory = tempfile::tempdir().expect("temp dir");
        let store = AcpRegistryStore::new(directory.path());
        let spec = store.launch_spec("cline").expect("installed Cline");
        assert_eq!(spec.registry_id, "cline");
        assert_eq!(spec.command, command);
        assert_eq!(spec.args, ["--acp"]);
        if let Some(version) = installed_package_version(&command) {
            assert_eq!(spec.version, version);
        }
    }

    #[test]
    fn rejects_duplicate_ids_traversal_and_insecure_urls() {
        let duplicate = FIXTURE.replace("\"fast-agent\"", "\"codex-acp\"");
        assert!(AcpRegistry::parse(duplicate.as_bytes()).is_err());
        let traversal = FIXTURE.replace("./bin/native", "../native");
        assert!(AcpRegistry::parse(traversal.as_bytes()).is_err());
        let insecure = FIXTURE.replace("https://example.com", "http://example.com");
        assert!(AcpRegistry::parse(insecure.as_bytes()).is_err());
        let option_injection = FIXTURE.replace("@acp/codex@1.2.3", "--package=evil@1.0.0");
        assert!(AcpRegistry::parse(option_injection.as_bytes()).is_err());
        let unpinned = FIXTURE.replace("@acp/codex@1.2.3", "@acp/codex");
        assert!(AcpRegistry::parse(unpinned.as_bytes()).is_err());
    }

    #[test]
    fn friendly_product_aliases_resolve_to_official_ids() {
        assert_eq!(canonical_agent_id("claude-code"), "claude-acp");
        assert_eq!(canonical_agent_id("codex"), "codex-acp");
        assert_eq!(canonical_agent_id("amp"), "amp-acp");
        assert_eq!(canonical_agent_id("kimi-code"), "kimi-code");
        assert_eq!(canonical_agent_id("kimi-cli"), "kimi");
        assert_eq!(canonical_agent_id("vibe-chat"), "mistral-vibe");
        assert_eq!(canonical_agent_id("antigravity"), "antigravity-acp");
        assert_eq!(canonical_agent_id("agy"), "antigravity-acp");
        assert_eq!(canonical_agent_id(" OpenCode "), "opencode");
        assert_eq!(
            agent_coordinate("claude-code", "0.66.0"),
            "claude-acp@0.66.0"
        );
        assert_eq!(agent_id_from_coordinate("vibe-chat@2.24.0"), "mistral-vibe");
    }

    #[test]
    fn antigravity_community_bridge_is_platform_pinned_and_verified() {
        let agent = community_acp_agent("antigravity-community").expect("community descriptor");
        assert_eq!(agent.id, "antigravity-community");
        assert!(community_acp_agent("antigravity").is_none());
        assert_eq!(agent.version, "1.0.0");
        assert!(agent.distribution.npx.is_none());
        assert!(agent.distribution.uvx.is_none());
        for (platform, binary) in &agent.distribution.binary {
            assert!(matches!(
                platform.as_str(),
                "darwin-aarch64" | "darwin-x86_64" | "linux-aarch64" | "linux-x86_64"
            ));
            assert!(binary.archive.starts_with(
                "https://github.com/shubzkothekar/antigravity-acp/releases/download/v1.0.0/"
            ));
            assert_eq!(binary.sha256.as_deref().map(str::len), Some(64));
            assert!(binary.cmd.starts_with("./agy-acp-"));
        }
        AcpRegistry {
            version: "community-pinned".to_string(),
            agents: vec![agent],
        }
        .validate()
        .expect("community descriptor obeys registry hardening");
    }

    #[test]
    fn antigravity_community_is_separate_from_the_official_registry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = AcpRegistryStore::new(dir.path());
        let resolved = store
            .resolve_agent("antigravity-community")
            .expect("built-in community descriptor");
        assert_eq!(resolved.id, "antigravity-community");
        assert_eq!(resolved.version, "1.0.0");
        let official = AcpRegistryAgent {
            id: "antigravity-acp".into(),
            name: "Google Antigravity".into(),
            version: "fixture".into(),
            description: String::new(),
            repository: String::new(),
            distribution: AcpDistribution {
                npx: Some(AcpPackageDistribution {
                    package: "official-fixture@1.0.0".into(),
                    args: Vec::new(),
                    env: BTreeMap::new(),
                }),
                ..AcpDistribution::default()
            },
        };
        atomic_write(
            &store.cache_path(),
            &serde_json::to_vec(&AcpRegistry {
                version: "fixture".into(),
                agents: vec![official],
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            store.resolve_agent("antigravity").unwrap().version,
            "fixture"
        );
    }

    #[test]
    fn official_google_publisher_check_rejects_other_origins_and_launch_overrides() {
        let mut binary = AcpBinaryDistribution {
            archive: "https://dl.google.com/agy-extensions/releases/macos/agy-acp-server-1.4.1-darwin-arm64.zip".into(),
            cmd: "./agy_acp_server.par".into(), sha256: None,
            args: Vec::new(), env: BTreeMap::new(),
        };
        assert!(official_google_binary("antigravity-acp", &binary));
        assert!(!official_google_binary("antigravity-community", &binary));
        binary.archive = binary
            .archive
            .replace("dl.google.com", "dl.google.com.example.test");
        assert!(!official_google_binary("antigravity-acp", &binary));
        binary.archive = binary
            .archive
            .replace("dl.google.com.example.test", "dl.google.com");
        binary.args.push("--arbitrary".into());
        assert!(!official_google_binary("antigravity-acp", &binary));
    }

    #[test]
    fn installed_agent_discovery_requires_the_verified_binary_marker() {
        let directory = tempfile::tempdir().unwrap();
        let store = AcpRegistryStore::new(directory.path());
        let binary = AcpBinaryDistribution {
            archive: "https://example.test/agent.tar.gz".into(),
            cmd: "./agent".into(),
            sha256: Some("a".repeat(64)),
            args: Vec::new(),
            env: BTreeMap::new(),
        };
        let agent = AcpRegistryAgent {
            id: "fixture-installed".into(),
            name: "Fixture".into(),
            version: "1.0.0".into(),
            description: String::new(),
            repository: String::new(),
            distribution: AcpDistribution {
                binary: [(current_platform().into(), binary)].into(),
                ..AcpDistribution::default()
            },
        };
        atomic_write(
            &store.cache_path(),
            &serde_json::to_vec(&AcpRegistry {
                version: "fixture".into(),
                agents: vec![agent.clone()],
            })
            .unwrap(),
        )
        .unwrap();
        let install = store.install_dir(&agent);
        atomic_write(&install.join("agent"), b"fixture executable").unwrap();
        assert!(!store
            .installed_binary_specs()
            .iter()
            .any(|spec| spec.registry_id == agent.id));
        atomic_write(&install.join(".archive.sha256"), "a".repeat(64).as_bytes()).unwrap();
        assert!(store
            .installed_binary_specs()
            .iter()
            .any(|spec| spec.registry_id == agent.id));
        store.cache_agent_snapshot(&agent).unwrap();
        let mut newer = agent.clone();
        newer.version = "2.0.0".into();
        atomic_write(
            &store.cache_path(),
            &serde_json::to_vec(&AcpRegistry {
                version: "newer".into(),
                agents: vec![newer],
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(store.launch_spec(&agent.id).unwrap().version, "1.0.0");
    }

    #[test]
    fn model_pinned_coordinates_split_additively() {
        // `#model` extends `id@version` without changing which agent resolves —
        // including through the alias table.
        assert_eq!(
            agent_coordinate_with_model("demo-acp", "1.2.3", "agent-model-1"),
            "demo-acp@1.2.3#agent-model-1"
        );
        assert_eq!(
            agent_coordinate_with_model("vibe-chat", "2.24.0", "agent-model-1"),
            "mistral-vibe@2.24.0#agent-model-1"
        );
        assert_eq!(
            agent_id_from_coordinate("demo-acp@1.2.3#agent-model-1"),
            "demo-acp"
        );
        assert_eq!(
            split_agent_coordinate("demo-acp@1.2.3#agent-model-1"),
            ("demo-acp".to_string(), Some("1.2.3".to_string()))
        );
        assert_eq!(
            agent_model_from_coordinate("demo-acp@1.2.3#agent-model-1"),
            Some("agent-model-1")
        );
        // Pre-pinning coordinates keep splitting exactly as before.
        assert_eq!(agent_model_from_coordinate("demo-acp@1.2.3"), None);
        assert_eq!(agent_model_from_coordinate("demo-acp"), None);
        // A dangling `#` pins nothing.
        assert_eq!(agent_model_from_coordinate("demo-acp@1.2.3#"), None);
        assert_eq!(agent_id_from_coordinate("demo-acp@1.2.3#"), "demo-acp");
    }

    #[test]
    fn connected_agent_snapshot_stays_version_pinned_after_catalog_refresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = AcpRegistryStore::new(dir.path());
        let registry = AcpRegistry::parse(FIXTURE.as_bytes()).expect("registry");
        let original = registry.get("codex-acp").expect("agent").clone();
        store
            .cache_agent_snapshot(&original)
            .expect("persist immutable snapshot");

        let refreshed = FIXTURE
            .replace("\"version\":\"1.2.3\"", "\"version\":\"9.9.9\"")
            .replace("@acp/codex@1.2.3", "@acp/codex@9.9.9");
        atomic_write(&store.cache_path(), refreshed.as_bytes()).expect("new latest cache");
        let pinned = store
            .resolve_agent("codex-acp@1.2.3")
            .expect("old connected version remains resolvable");
        assert_eq!(pinned.version, "1.2.3");
        assert_eq!(pinned.distribution.npx.unwrap().package, "@acp/codex@1.2.3");
        assert_eq!(store.resolve_agent("codex-acp").unwrap().version, "9.9.9");
    }

    /// PR #68 review: registry validation still admits a version containing a
    /// path separator, and the install collapsed it into a safe directory name
    /// while `resolve_agent` looked the raw string up as a path — so such an
    /// agent installed successfully and then immediately resolved as
    /// `NotInstalled`. Both sides now go through `agent_dir`.
    #[test]
    fn a_version_with_a_separator_installs_and_resolves_at_the_same_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = AcpRegistryStore::new(dir.path());
        let registry = AcpRegistry::parse(FIXTURE.as_bytes()).expect("registry");

        let mut agent = registry.get("codex-acp").expect("agent").clone();
        agent.version = "release/1.2.3".to_string();
        AcpRegistry {
            version: "snapshot".to_string(),
            agents: vec![agent.clone()],
        }
        .validate()
        .expect("validation still admits a separator in a version");

        store
            .cache_agent_snapshot(&agent)
            .expect("install writes the snapshot");
        // Written where the sanitizer put it — not `agents/codex-acp/release/1.2.3/`.
        assert!(store
            .root
            .join("agents")
            .join("codex-acp")
            .join("release_1.2.3")
            .join(".registry-agent.json")
            .is_file());

        let resolved = store
            .resolve_agent(&agent_coordinate(&agent.id, &agent.version))
            .expect("an installed agent resolves right back");
        assert_eq!(resolved.version, "release/1.2.3");
        assert_eq!(resolved.id, "codex-acp");

        // The shared sanitizer collapses rather than trusts, so nothing routed
        // through it can escape the agents directory on either path.
        for hostile in ["../../etc", "a\\b", "a/../b", ".."] {
            let component = safe_path_component(hostile);
            assert!(
                !component.contains('/') && !component.contains('\\') && !component.contains(".."),
                "`{hostile}` sanitized to `{component}`, which is still a path"
            );
        }
    }

    #[test]
    fn package_manager_entries_do_not_require_binary_install() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = AcpRegistryStore::new(dir.path());
        atomic_write(&store.cache_path(), FIXTURE.as_bytes()).expect("cache fixture");
        assert!(matches!(
            store.launch_spec("codex-acp"),
            Ok(_) | Err(AcpRegistryError::ToolUnavailable { .. })
        ));
        let native = store.launch_spec("native");
        if current_platform() == "linux-x86_64" {
            assert!(matches!(native, Err(AcpRegistryError::NotInstalled { .. })));
        } else {
            assert!(matches!(
                native,
                Err(AcpRegistryError::UnsupportedPlatform { .. })
            ));
        }
    }

    #[tokio::test]
    async fn automatic_discovery_reuses_a_fresh_validated_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = AcpRegistryStore::new(dir.path());
        atomic_write(&store.cache_path(), FIXTURE.as_bytes()).expect("cache fixture");
        let registry = store.load_or_refresh().await.expect("fresh cache");
        assert_eq!(registry.version, "1.0.0");
        assert_eq!(registry.agents.len(), 3);
    }

    #[test]
    fn raw_binary_install_is_bounded_to_the_declared_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("agent");
        install_archive(
            &target,
            "https://example.com/agent",
            "./bin/agent",
            &"a".repeat(64),
            b"agent",
        )
        .expect("raw binary install");
        assert_eq!(std::fs::read(target.join("bin/agent")).unwrap(), b"agent");
        assert!(!dir.path().join("bin/agent").exists());
    }

    #[test]
    fn tar_bz2_install_requires_the_declared_command() {
        let mut tar_bytes = Vec::new();
        {
            let encoder = bzip2::write::BzEncoder::new(&mut tar_bytes, bzip2::Compression::best());
            let mut archive = tar::Builder::new(encoder);
            let payload = b"#!/bin/sh\n";
            let mut header = tar::Header::new_gnu();
            header.set_path("bin/agent").unwrap();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            archive.append(&header, payload.as_slice()).unwrap();
            let encoder = archive.into_inner().unwrap();
            encoder.finish().unwrap();
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("agent");
        install_archive(
            &target,
            "https://example.com/agent.tar.bz2",
            "./bin/agent",
            &"a".repeat(64),
            &tar_bytes,
        )
        .expect("tar.bz2 install");
        assert!(target.join("bin/agent").is_file());

        let missing = dir.path().join("missing");
        assert!(install_archive(
            &missing,
            "https://example.com/agent.tar.bz2",
            "./bin/not-there",
            &"a".repeat(64),
            &tar_bytes,
        )
        .is_err());
        assert!(!missing.exists());
    }

    #[cfg(unix)]
    #[test]
    fn tar_allows_deferred_in_root_symlinks_but_rejects_escape_targets() {
        fn archive_with_link(target: &str) -> Vec<u8> {
            let mut bytes = Vec::new();
            {
                let mut archive = tar::Builder::new(&mut bytes);
                let payload = b"runtime";
                let mut file = tar::Header::new_gnu();
                file.set_path("lib/real").unwrap();
                file.set_size(payload.len() as u64);
                file.set_mode(0o600);
                file.set_cksum();
                archive.append(&file, payload.as_slice()).unwrap();

                let mut link = tar::Header::new_gnu();
                link.set_path("lib/current").unwrap();
                link.set_size(0);
                link.set_mode(0o777);
                link.set_entry_type(tar::EntryType::Symlink);
                link.set_link_name(target).unwrap();
                link.set_cksum();
                archive.append(&link, std::io::empty()).unwrap();
                archive.finish().unwrap();
            }
            bytes
        }

        let dir = tempfile::tempdir().expect("tempdir");
        extract_tar(Cursor::new(archive_with_link("real")), dir.path()).expect("safe link");
        assert_eq!(
            std::fs::read(dir.path().join("lib/current")).unwrap(),
            b"runtime"
        );

        let escaped = tempfile::tempdir().expect("tempdir");
        assert!(extract_tar(
            Cursor::new(archive_with_link("../../../outside")),
            escaped.path()
        )
        .is_err());
        assert!(!escaped.path().parent().unwrap().join("outside").exists());
    }

    #[test]
    fn zip_traversal_is_rejected_without_writing_outside_the_stage() {
        let mut zip_bytes = Cursor::new(Vec::new());
        {
            let mut archive = zip::ZipWriter::new(&mut zip_bytes);
            archive
                .start_file("../escaped", zip::write::SimpleFileOptions::default())
                .unwrap();
            archive.write_all(b"bad").unwrap();
            archive.finish().unwrap();
        }
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(extract_zip(zip_bytes.get_ref(), dir.path()).is_err());
        assert!(!dir.path().parent().unwrap().join("escaped").exists());
    }
}
