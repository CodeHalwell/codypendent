//! Executing skill `scripts/` through the OS sandbox (STEP 6.4).
//!
//! Phase 2 recorded a skill's `scripts/` but refused to run them — the registry
//! marked a script-bearing skill non-executable because no sandbox existed to
//! confine the script. STEP 6.2 built that sandbox
//! ([`codypendent_sandbox::executor`]), so this module lifts the restriction: a
//! skill's declared `[permissions]` lower into a closed
//! [`SandboxProfile`](codypendent_sandbox::SandboxProfile), and a named script
//! runs through a [`SandboxExecutor`](codypendent_sandbox::SandboxExecutor) —
//! confined, output captured, sanitized, and origin-labeled.
//!
//! This is the skill half of STEP 6.4. The hook engine (the `validate`-kind
//! command hook of [`specs/hook.toml`](../../../docs/specs/hook.toml)) is a
//! separate, larger build that does not yet exist in the codebase; it is scoped as
//! a follow-up rather than stubbed here.
//!
//! The mapping from a skill's permissions to a profile:
//!
//! | `[permissions]`   | profile field         |
//! |-------------------|-----------------------|
//! | `filesystem_read` | `read_paths`          |
//! | `filesystem_write`| `write_paths`         |
//! | `network`         | `network_allowlist`   |
//! | `secrets`         | `brokered_secrets`    |
//! | `commands` (any)  | `allow_subprocess`    |
//!
//! Permission *values* are taken verbatim (Phase-2 placeholders such as
//! `$REPOSITORY` / `$WORKTREE` are not yet substituted — a documented follow-up;
//! a non-absolute grant is simply ineffective at the OS layer, which fails closed,
//! not open). The script's own directory is always granted read so the sandbox can
//! read and execute the script itself.

use std::path::Path;

use codypendent_sandbox::{
    SandboxCommand, SandboxError, SandboxExecutor, SandboxOutcome, SandboxProfile, ENV_ALLOWLIST,
};

use crate::types::CapabilityRequest;

/// Conservative resource defaults for a skill script that declares no explicit
/// limits (a skill package's `[limits]` are not yet plumbed into a profile).
const DEFAULT_MEMORY_MB: u64 = 128;
const DEFAULT_CPU_SECONDS: u64 = 30;
const DEFAULT_WALL_SECONDS: u64 = 60;
const DEFAULT_OUTPUT_MB: u64 = 8;

/// A failure resolving or running a skill script.
#[derive(Debug, thiserror::Error)]
pub enum SkillExecError {
    /// The named script does not exist under the skill's package directory.
    #[error("skill script `{0}` not found under the package directory")]
    ScriptNotFound(String),
    /// The resolved script path escapes the skill package directory (a `../` or
    /// symlink traversal) — refused so a skill can only run its own bundled code.
    #[error("skill script `{path}` escapes the package directory")]
    ScriptEscapesPackage {
        /// The offending relative path.
        path: String,
    },
    /// The script is not an executable file (no execute bit).
    #[error("skill script `{0}` is not an executable file (chmod +x and add a shebang)")]
    ScriptNotExecutable(String),
    /// The sandbox refused or failed the run (fails closed).
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
    /// An I/O error resolving the script path.
    #[error("resolving skill script: {0}")]
    Io(#[from] std::io::Error),
}

/// Lower a skill's flattened `[permissions]` into a closed [`SandboxProfile`].
///
/// `label` becomes the profile's plugin/origin tag (e.g. `skill:rust.fix-ci`).
/// Any [`Command`](CapabilityRequest::Command) permission grants `allow_subprocess`
/// (a skill that lists commands runs them). Resource caps use conservative
/// defaults except `wall_seconds`, which the caller supplies.
#[must_use]
pub fn profile_for_permissions(
    label: impl Into<String>,
    permissions: &[CapabilityRequest],
    wall_seconds: u64,
) -> SandboxProfile {
    let mut read_paths = Vec::new();
    let mut write_paths = Vec::new();
    let mut network_allowlist = Vec::new();
    let mut brokered_secrets = Vec::new();
    let mut allow_subprocess = false;
    for perm in permissions {
        match perm {
            CapabilityRequest::FilesystemRead(p) => read_paths.push(p.clone()),
            CapabilityRequest::FilesystemWrite(p) => write_paths.push(p.clone()),
            CapabilityRequest::Network(n) => network_allowlist.push(n.clone()),
            CapabilityRequest::Secret(s) => brokered_secrets.push(s.clone()),
            // A declared command capability means the script may spawn subprocesses.
            CapabilityRequest::Command(_) => allow_subprocess = true,
        }
    }
    SandboxProfile {
        plugin: label.into(),
        env_allowlist: ENV_ALLOWLIST.iter().map(|s| (*s).to_string()).collect(),
        read_paths,
        write_paths,
        network_allowlist,
        brokered_secrets,
        allow_subprocess,
        memory_mb: DEFAULT_MEMORY_MB,
        cpu_seconds: DEFAULT_CPU_SECONDS,
        wall_seconds: if wall_seconds == 0 {
            DEFAULT_WALL_SECONDS
        } else {
            wall_seconds
        },
        maximum_output_mb: DEFAULT_OUTPUT_MB,
    }
}

/// Run a skill's script through the sandbox `executor`.
///
/// * `skill_dir` — the skill package directory (the [`Provenance::Package`] path).
/// * `script_relpath` — the script's path relative to `skill_dir` (e.g.
///   `scripts/fix.sh`).
/// * `profile` — the profile derived from the skill's permissions
///   ([`profile_for_permissions`]). The script's own directory is granted read
///   automatically so the sandbox can read and execute it.
///
/// Fails closed: a missing/non-executable script, a path escaping the package, or
/// an unavailable/unsupported sandbox all refuse to run. Returns the sanitized,
/// origin-labeled [`SandboxOutcome`].
///
/// [`Provenance::Package`]: crate::types::Provenance::Package
pub fn run_script(
    executor: &dyn SandboxExecutor,
    skill_dir: &Path,
    script_relpath: &str,
    args: Vec<String>,
    profile: &SandboxProfile,
) -> Result<SandboxOutcome, SkillExecError> {
    let root = skill_dir.canonicalize()?;
    let candidate = root.join(script_relpath);
    let script = candidate
        .canonicalize()
        .map_err(|_| SkillExecError::ScriptNotFound(script_relpath.to_string()))?;
    // The script must stay inside the package — a skill runs only its own code.
    if !script.starts_with(&root) {
        return Err(SkillExecError::ScriptEscapesPackage {
            path: script_relpath.to_string(),
        });
    }
    if !is_executable_file(&script) {
        return Err(SkillExecError::ScriptNotExecutable(
            script_relpath.to_string(),
        ));
    }

    // Grant read on the package root so the sandbox can read+execute the script
    // (and any bundled references it reads) and use the package as its cwd.
    let mut confined = profile.clone();
    let root_str = root.to_string_lossy().into_owned();
    if !confined.read_paths.iter().any(|p| p == &root_str) {
        confined.read_paths.push(root_str);
    }

    let origin = if confined.plugin.is_empty() {
        "skill".to_string()
    } else {
        confined.plugin.clone()
    };
    let command = SandboxCommand::new(script, args, root, origin);
    Ok(executor.run(&confined, &command)?)
}

/// Whether `path` is a regular file with an execute bit.
fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(path)
            .map(|m| m.is_file())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_lower_into_a_closed_profile() {
        let perms = vec![
            CapabilityRequest::FilesystemRead("/repo".into()),
            CapabilityRequest::FilesystemWrite("/repo/target".into()),
            CapabilityRequest::Network("api.github.com:443".into()),
            CapabilityRequest::Secret("github-token".into()),
            CapabilityRequest::Command("cargo".into()),
        ];
        let p = profile_for_permissions("skill:rust.fix-ci", &perms, 120);
        assert_eq!(p.plugin, "skill:rust.fix-ci");
        assert_eq!(p.read_paths, ["/repo"]);
        assert_eq!(p.write_paths, ["/repo/target"]);
        assert_eq!(p.network_allowlist, ["api.github.com:443"]);
        assert_eq!(p.brokered_secrets, ["github-token"]);
        assert!(p.allow_subprocess, "a command permission grants subprocess");
        assert_eq!(p.wall_seconds, 120);
    }

    #[test]
    fn no_command_permission_means_no_subprocess() {
        let perms = vec![CapabilityRequest::FilesystemRead("/repo".into())];
        let p = profile_for_permissions("skill:x", &perms, 0);
        assert!(!p.allow_subprocess);
        // A zero wall falls back to the conservative default, never "no limit".
        assert_eq!(p.wall_seconds, DEFAULT_WALL_SECONDS);
    }

    #[test]
    fn a_missing_script_fails_closed() {
        use codypendent_sandbox::RefusingSandbox;
        let dir = tempfile::tempdir().unwrap();
        let profile = profile_for_permissions("skill:x", &[], 30);
        let err = run_script(
            &RefusingSandbox,
            dir.path(),
            "scripts/does-not-exist.sh",
            Vec::new(),
            &profile,
        )
        .unwrap_err();
        assert!(matches!(err, SkillExecError::ScriptNotFound(_)));
    }

    #[test]
    fn a_non_executable_script_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
        let script = dir.path().join("scripts").join("noexec.sh");
        std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
        // No execute bit set.
        use codypendent_sandbox::RefusingSandbox;
        let profile = profile_for_permissions("skill:x", &[], 30);
        let err = run_script(
            &RefusingSandbox,
            dir.path(),
            "scripts/noexec.sh",
            Vec::new(),
            &profile,
        )
        .unwrap_err();
        assert!(matches!(err, SkillExecError::ScriptNotExecutable(_)));
    }

    #[test]
    fn an_executable_script_reaches_the_executor_then_fails_closed_off_backend() {
        // With an executable script present, resolution succeeds and the call
        // reaches the executor — which, being the refusing (no-backend) executor,
        // fails closed. This proves the resolve→execute path without depending on a
        // platform backend.
        use codypendent_sandbox::RefusingSandbox;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
        let script = dir.path().join("scripts").join("run.sh");
        std::fs::write(&script, "#!/bin/sh\necho ran\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let profile = profile_for_permissions("skill:x", &[], 30);
        let err = run_script(
            &RefusingSandbox,
            dir.path(),
            "scripts/run.sh",
            Vec::new(),
            &profile,
        )
        .unwrap_err();
        // Reached the executor and was refused (fail closed), not a resolution error.
        assert!(matches!(err, SkillExecError::Sandbox(_)));
    }
}
