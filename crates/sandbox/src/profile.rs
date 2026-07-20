//! Sandbox-profile derivation (STEP 6.2, decision layer).
//!
//! The cross-cutting note in the roadmap frames the policy engine as *"the
//! compiler that emits a sandbox profile"*: it **decides** (deny / allow /
//! approve) and lowers a plugin's granted capabilities into a concrete
//! [`SandboxProfile`] — the clean environment, the pre-opened paths, the network
//! allowlist, the resource limits, the wall-clock kill, the output cap. The
//! actual OS enforcement (bubblewrap + seccomp on Linux, `sandbox-exec` on macOS,
//! AppContainer on Windows, `wasmtime` fuel/memory for WASM) is the executor that
//! *consumes* this profile; deriving it here is pure and testable, and the profile
//! is the audited contract between the two.
//!
//! The profile is deliberately **closed**: it lists exactly what is allowed and
//! nothing else. An executor that honours it cannot grant access the profile does
//! not name, so an undeclared path or host fails structurally (exit criterion 1).

use crate::manifest::PluginManifest;
use crate::permission::CapabilitySet;

/// The environment variables a sandboxed plugin may inherit — a *closed*
/// allowlist. Everything else is stripped, so a secret sitting in the daemon's
/// environment (an API token canary) is invisible to the plugin
/// ([Chapter 11](../../docs/docs/11-security-and-governance.md): no secrets in env).
pub const ENV_ALLOWLIST: &[&str] = &["PATH", "LANG", "LC_ALL", "TZ"];

/// A derived, closed sandbox profile: exactly what the plugin may touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProfile {
    /// The plugin this profile isolates (`id@version`).
    pub plugin: String,
    /// The environment variables that survive into the sandbox (allowlist only).
    pub env_allowlist: Vec<String>,
    /// Paths pre-opened read-only. The plugin sees no other part of the filesystem.
    pub read_paths: Vec<String>,
    /// Paths pre-opened read-write.
    pub write_paths: Vec<String>,
    /// `host:port` destinations reachable. Empty ⇒ network fully denied.
    pub network_allowlist: Vec<String>,
    /// Named secrets brokered per call (never placed in env).
    pub brokered_secrets: Vec<String>,
    /// Whether the plugin may spawn subprocesses.
    pub allow_subprocess: bool,
    /// Memory ceiling (rlimit).
    pub memory_mb: u64,
    /// CPU-time ceiling (rlimit).
    pub cpu_seconds: u64,
    /// Wall-clock ceiling — the plugin is killed (process-group) past this.
    pub wall_seconds: u64,
    /// Captured-output ceiling; output beyond this is truncated to an artifact.
    pub maximum_output_mb: u64,
}

impl SandboxProfile {
    /// Derive the profile from a plugin manifest and the capabilities that were
    /// actually granted (the granted set may be *narrower* than the manifest
    /// requested — a user can enable a plugin while withholding a capability, and
    /// the profile reflects the grant, not the request).
    #[must_use]
    pub fn derive(manifest: &PluginManifest, granted: &CapabilitySet) -> Self {
        use crate::permission::Capability::*;

        let mut read_paths = Vec::new();
        let mut write_paths = Vec::new();
        let mut network_allowlist = Vec::new();
        let mut brokered_secrets = Vec::new();
        let mut allow_subprocess = false;
        for cap in granted.iter() {
            match cap {
                FilesystemRead(p) => read_paths.push(p.clone()),
                FilesystemWrite(p) => write_paths.push(p.clone()),
                Network(n) => network_allowlist.push(n.clone()),
                Secret(s) => brokered_secrets.push(s.clone()),
                Subprocess => allow_subprocess = true,
            }
        }

        Self {
            plugin: format!("{}@{}", manifest.id, manifest.version),
            env_allowlist: ENV_ALLOWLIST.iter().map(|s| s.to_string()).collect(),
            read_paths,
            write_paths,
            network_allowlist,
            brokered_secrets,
            allow_subprocess,
            memory_mb: manifest.resources.memory_mb,
            cpu_seconds: manifest.resources.cpu_seconds,
            wall_seconds: manifest.resources.wall_seconds,
            maximum_output_mb: manifest.resources.maximum_output_mb,
        }
    }

    /// Whether the profile permits reading `path`. The check an executor makes
    /// before opening a file on the plugin's behalf: a path not under any
    /// pre-opened read (or write) path is denied.
    #[must_use]
    pub fn allows_read(&self, path: &str) -> bool {
        self.read_paths
            .iter()
            .chain(self.write_paths.iter())
            .any(|p| path_within(p, path))
    }

    /// Whether the profile permits writing `path`.
    #[must_use]
    pub fn allows_write(&self, path: &str) -> bool {
        self.write_paths.iter().any(|p| path_within(p, path))
    }

    /// Whether the profile permits reaching `host:port`. Empty allowlist ⇒ all
    /// network denied.
    #[must_use]
    pub fn allows_network(&self, host_port: &str) -> bool {
        self.network_allowlist.iter().any(|h| h == host_port)
    }

    /// Whether an environment variable survives into the sandbox.
    #[must_use]
    pub fn env_allowed(&self, var: &str) -> bool {
        self.env_allowlist.iter().any(|v| v == var)
    }
}

/// Whether `candidate` is `base` itself or a path beneath it. String-prefix
/// containment with a separator guard (so `/a/bc` is not "within" `/a/b`); the
/// executor canonicalizes real paths first, this is the profile-level check.
fn path_within(base: &str, candidate: &str) -> bool {
    let base = base.trim_end_matches('/');
    if candidate == base {
        return true;
    }
    match candidate.strip_prefix(base) {
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse_manifest;

    fn manifest() -> PluginManifest {
        parse_manifest(
            r#"
schema_version = 1
id = "github"
name = "GitHub"
version = "0.1.0"
kind = "native-process"
publisher = "codypendent-project"
[runtime]
command = "codypendent-plugin-github"
[capabilities]
filesystem_read = ["/workspace/repo"]
filesystem_write = ["/workspace/repo/target"]
network = ["api.github.com:443"]
secrets = ["github-token"]
subprocess = false
[resources]
memory_mb = 256
cpu_seconds = 60
wall_seconds = 120
maximum_output_mb = 20
"#,
        )
        .unwrap()
    }

    #[test]
    fn derives_closed_profile_from_grant() {
        let m = manifest();
        let granted = CapabilitySet::from_spec(&m.capabilities);
        let p = SandboxProfile::derive(&m, &granted);
        assert_eq!(p.plugin, "github@0.1.0");
        assert_eq!(p.network_allowlist, ["api.github.com:443"]);
        assert_eq!(p.brokered_secrets, ["github-token"]);
        assert!(!p.allow_subprocess);
        assert_eq!(p.memory_mb, 256);
        assert_eq!(p.wall_seconds, 120);
    }

    #[test]
    fn undeclared_path_and_host_are_denied() {
        let m = manifest();
        let granted = CapabilitySet::from_spec(&m.capabilities);
        let p = SandboxProfile::derive(&m, &granted);
        // The exit-criterion-1 shape: reading ~/.ssh and a non-allowlisted host fail.
        assert!(!p.allows_read("/home/user/.ssh/id_rsa"));
        assert!(!p.allows_network("evil.example.com:443"));
        // Granted access does work.
        assert!(p.allows_read("/workspace/repo/src/lib.rs"));
        assert!(p.allows_write("/workspace/repo/target/debug/x"));
        assert!(p.allows_network("api.github.com:443"));
    }

    #[test]
    fn write_path_grants_read_but_not_the_reverse() {
        let m = manifest();
        let granted = CapabilitySet::from_spec(&m.capabilities);
        let p = SandboxProfile::derive(&m, &granted);
        // read_paths does not include target; write_paths does. A write path is
        // readable; a read-only path is not writable.
        assert!(p.allows_read("/workspace/repo/target/x"));
        assert!(!p.allows_write("/workspace/repo/src/lib.rs"));
    }

    #[test]
    fn env_canary_is_stripped() {
        let m = manifest();
        let granted = CapabilitySet::from_spec(&m.capabilities);
        let p = SandboxProfile::derive(&m, &granted);
        assert!(p.env_allowed("PATH"));
        // A secret in the daemon's environment is not inherited.
        assert!(!p.env_allowed("AWS_SECRET_ACCESS_KEY"));
        assert!(!p.env_allowed("GITHUB_TOKEN"));
    }

    #[test]
    fn narrowed_grant_narrows_the_profile() {
        let m = manifest();
        // The user enabled the plugin but withheld the secret and network.
        let narrowed = CapabilitySet::from_spec(&crate::manifest::CapabilitiesSpec {
            filesystem_read: vec!["/workspace/repo".into()],
            filesystem_write: vec![],
            network: vec![],
            secrets: vec![],
            subprocess: false,
        });
        let p = SandboxProfile::derive(&m, &narrowed);
        assert!(p.network_allowlist.is_empty());
        assert!(p.brokered_secrets.is_empty());
        assert!(!p.allows_network("api.github.com:443"));
    }

    #[test]
    fn path_within_guards_separator() {
        assert!(path_within("/a/b", "/a/b"));
        assert!(path_within("/a/b", "/a/b/c"));
        assert!(!path_within("/a/b", "/a/bc"));
        assert!(!path_within("/a/b", "/a"));
    }
}
