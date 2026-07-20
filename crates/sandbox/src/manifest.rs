//! Plugin manifests (STEP 6.1): the parsed shape of `docs/specs/plugin.toml`.
//!
//! A plugin declares its identity, the runtime that hosts it, the *capabilities*
//! it needs (filesystem, network, secrets, subprocess), the *resources* it may
//! consume, its *security* record (checksum, signature, sandbox profile), and its
//! *update* policy. This module is the parser and validator; verification lives
//! in [`crate::verify`], the capability model in [`crate::permission`], and the
//! lifecycle in [`crate::lifecycle`]. Nothing here executes a plugin — the
//! manifest is untrusted input that every later stage gates on.

use serde::{Deserialize, Serialize};

/// The plugin-manifest schema version this build understands.
pub const SUPPORTED_PLUGIN_SCHEMA_VERSION: u32 = 1;

/// A parse/validation failure for a plugin manifest.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("invalid plugin manifest: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("unsupported plugin schema_version {found} (this build supports {supported})")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("plugin id must not be empty")]
    EmptyId,
    #[error("plugin version must not be empty")]
    EmptyVersion,
    #[error("plugin publisher must not be empty")]
    EmptyPublisher,
    #[error("plugin runtime command must not be empty for a {kind} plugin")]
    EmptyCommand { kind: &'static str },
}

/// The kind of runtime that hosts a plugin.
///
/// The three classes Chapter 05 admits: a sandboxed OS process speaking a wire
/// protocol, a WASM component loaded into the daemon, or a remote MCP endpoint
/// reached over the network. The kind selects which isolation mechanism the
/// sandbox applies — but *every* kind declares capabilities, and undeclared
/// access fails regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginKind {
    /// A native OS process, isolated by the platform sandbox.
    NativeProcess,
    /// A WASM component loaded into the daemon's `wasmtime` runtime.
    WasmComponent,
    /// A remote MCP server reached over the network (still capability-gated).
    McpRemote,
}

impl PluginKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PluginKind::NativeProcess => "native-process",
            PluginKind::WasmComponent => "wasm-component",
            PluginKind::McpRemote => "mcp-remote",
        }
    }
}

/// A parsed plugin manifest (`plugin.toml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub schema_version: u32,
    /// The plugin's stable id (e.g. `github`).
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: PluginKind,
    /// The publisher identity — the key by which a signature is trusted.
    pub publisher: String,
    /// The scopes this plugin may be installed at.
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub runtime: RuntimeSpec,
    #[serde(default)]
    pub capabilities: CapabilitiesSpec,
    #[serde(default)]
    pub resources: ResourcesSpec,
    #[serde(default)]
    pub security: SecuritySpec,
    #[serde(default)]
    pub update: UpdateSpec,
}

/// How the plugin is started.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSpec {
    /// The command (native process) or component path (WASM) to launch.
    #[serde(default)]
    pub command: String,
    /// The wire protocol the runtime speaks (e.g. `mcp-stdio`).
    #[serde(default)]
    pub protocol: String,
    /// Working-directory policy (`isolated` = a fresh pre-opened dir only).
    #[serde(default)]
    pub working_directory: String,
}

/// The capabilities a plugin declares it needs. Anything not listed here is
/// denied at run time — the manifest is the *complete* statement of what the
/// plugin may touch (STEP 6.1 / exit criterion 1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesSpec {
    /// Filesystem paths the plugin may read (pre-opened; nothing else is visible).
    #[serde(default)]
    pub filesystem_read: Vec<String>,
    /// Filesystem paths the plugin may write.
    #[serde(default)]
    pub filesystem_write: Vec<String>,
    /// `host:port` network destinations the plugin may reach (an allowlist).
    #[serde(default)]
    pub network: Vec<String>,
    /// Named secrets the broker may pass to the plugin (never via env).
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Whether the plugin may spawn subprocesses.
    #[serde(default)]
    pub subprocess: bool,
}

/// Resource caps enforced by the sandbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourcesSpec {
    pub memory_mb: u64,
    pub cpu_seconds: u64,
    pub wall_seconds: u64,
    pub maximum_output_mb: u64,
}

impl Default for ResourcesSpec {
    fn default() -> Self {
        // Conservative defaults for a manifest that omits `[resources]`: a plugin
        // gets a small, bounded slice unless it asks for more (and is granted it).
        Self {
            memory_mb: 128,
            cpu_seconds: 30,
            wall_seconds: 60,
            maximum_output_mb: 8,
        }
    }
}

/// The plugin's security record: how the artifact is identified and trusted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecuritySpec {
    /// `sha256:<hex>` of the plugin artifact. Verified before install.
    #[serde(default)]
    pub checksum: String,
    /// Base64 ed25519 signature over the checksum, or a placeholder when unsigned.
    #[serde(default)]
    pub signature: String,
    /// The named sandbox profile the plugin runs under.
    #[serde(default)]
    pub sandbox_profile: String,
}

impl SecuritySpec {
    /// Whether the manifest carries a real (non-placeholder) signature. The
    /// packaging placeholders in `docs/specs/plugin.toml`
    /// (`set-during-packaging`) count as unsigned.
    #[must_use]
    pub fn is_signed(&self) -> bool {
        let sig = self.signature.trim();
        !sig.is_empty() && sig != "set-during-packaging"
    }
}

/// The plugin's update policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSpec {
    /// The release channel (e.g. `stable`).
    #[serde(default)]
    pub channel: String,
    /// Whether a permission change on update requires re-approval. Defaults to
    /// `true` — the safe posture; a manifest cannot silently opt out of the
    /// permission-diff gate (STEP 6.1 / exit criterion 2 is enforced in
    /// [`crate::lifecycle`] regardless of this flag).
    #[serde(default = "default_true")]
    pub permission_change_requires_approval: bool,
}

impl Default for UpdateSpec {
    fn default() -> Self {
        Self {
            channel: String::new(),
            permission_change_requires_approval: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Parse a plugin manifest from TOML and validate its schema version + required
/// identity fields. Does **not** verify checksum/signature or evaluate
/// permissions — those are separate, later lifecycle stages.
pub fn parse_manifest(toml_str: &str) -> Result<PluginManifest, ManifestError> {
    let manifest: PluginManifest = toml::from_str(toml_str)?;
    if manifest.schema_version != SUPPORTED_PLUGIN_SCHEMA_VERSION {
        return Err(ManifestError::UnsupportedSchemaVersion {
            found: manifest.schema_version,
            supported: SUPPORTED_PLUGIN_SCHEMA_VERSION,
        });
    }
    if manifest.id.trim().is_empty() {
        return Err(ManifestError::EmptyId);
    }
    if manifest.version.trim().is_empty() {
        return Err(ManifestError::EmptyVersion);
    }
    if manifest.publisher.trim().is_empty() {
        return Err(ManifestError::EmptyPublisher);
    }
    // A process/WASM plugin must say what to launch; a remote MCP plugin is
    // reached over its declared network allowlist instead.
    if matches!(
        manifest.kind,
        PluginKind::NativeProcess | PluginKind::WasmComponent
    ) && manifest.runtime.command.trim().is_empty()
    {
        return Err(ManifestError::EmptyCommand {
            kind: manifest.kind.as_str(),
        });
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GITHUB_MANIFEST: &str = r#"
schema_version = 1
id = "github"
name = "GitHub Integration"
version = "0.1.0"
kind = "native-process"
publisher = "codypendent-project"
scopes = ["user", "organization", "repository"]

[runtime]
command = "codypendent-plugin-github"
protocol = "mcp-stdio"
working_directory = "isolated"

[capabilities]
filesystem_read = []
filesystem_write = []
network = ["api.github.com:443", "uploads.github.com:443"]
secrets = ["github-token"]
subprocess = false

[resources]
memory_mb = 256
cpu_seconds = 60
wall_seconds = 120
maximum_output_mb = 20

[security]
checksum = "sha256:set-during-packaging"
signature = "set-during-packaging"
sandbox_profile = "network-client"

[update]
channel = "stable"
permission_change_requires_approval = true
"#;

    #[test]
    fn parses_the_canonical_github_manifest() {
        let m = parse_manifest(GITHUB_MANIFEST).expect("canonical manifest parses");
        assert_eq!(m.id, "github");
        assert_eq!(m.kind, PluginKind::NativeProcess);
        assert_eq!(
            m.capabilities.network,
            ["api.github.com:443", "uploads.github.com:443"]
        );
        assert_eq!(m.capabilities.secrets, ["github-token"]);
        assert!(!m.capabilities.subprocess);
        assert_eq!(m.resources.memory_mb, 256);
        assert!(
            !m.security.is_signed(),
            "packaging placeholder is not a signature"
        );
        assert!(m.update.permission_change_requires_approval);
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let bad = GITHUB_MANIFEST.replace("schema_version = 1", "schema_version = 99");
        assert!(matches!(
            parse_manifest(&bad),
            Err(ManifestError::UnsupportedSchemaVersion { found: 99, .. })
        ));
    }

    #[test]
    fn rejects_unknown_field() {
        let bad = format!("{GITHUB_MANIFEST}\nmalicious = true\n");
        assert!(matches!(parse_manifest(&bad), Err(ManifestError::Parse(_))));
    }

    #[test]
    fn rejects_process_plugin_without_a_command() {
        let bad =
            GITHUB_MANIFEST.replace("command = \"codypendent-plugin-github\"", "command = \"\"");
        assert!(matches!(
            parse_manifest(&bad),
            Err(ManifestError::EmptyCommand { .. })
        ));
    }

    #[test]
    fn resources_default_when_absent() {
        let minimal = r#"
schema_version = 1
id = "wc"
name = "Word Count"
version = "0.1.0"
kind = "wasm-component"
publisher = "me"
[runtime]
command = "word_count.wasm"
"#;
        let m = parse_manifest(minimal).expect("minimal wasm manifest parses");
        assert_eq!(m.resources, ResourcesSpec::default());
        assert!(
            m.update.permission_change_requires_approval,
            "approval gate defaults on"
        );
    }

    #[test]
    fn signature_placeholder_detection() {
        let mut sec = SecuritySpec::default();
        assert!(!sec.is_signed());
        sec.signature = "set-during-packaging".into();
        assert!(!sec.is_signed());
        sec.signature = "  ".into();
        assert!(!sec.is_signed());
        sec.signature = "aGVsbG8=".into();
        assert!(sec.is_signed());
    }
}
