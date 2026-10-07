//! Sandboxed, out-of-process runtime for TypeScript/React UI components.
//!
//! Component JavaScript is never loaded into the daemon. A verified
//! `ui-component` package is lowered through [`codypendent_sandbox`], spawned
//! with a clean environment, and speaks only length-framed
//! [`UiWireMessage`](codypendent_protocol::UiWireMessage) values over stdio.
//! This module owns process and transport health; document state and
//! contribution routing remain in the trusted host session/store layers.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use codypendent_protocol::{
    UiActionResult, UiCapabilities, UiCapabilitySelection, UiDispose, UiDocumentId, UiEvent,
    UiHardLimits, UiHotReload, UiProjectionUpdate, UiProtocolVersion, UiResyncRequest, UiRevision,
    UiViewport, UiWireMessage, UI_WORKER_MESSAGE_BURST, UI_WORKER_MESSAGE_RATE_PER_SECOND,
};
use codypendent_sandbox::{
    checksum_of, enforcing_executor, sanitize_untrusted, CapabilitySet, InstalledPlugin,
    LifecycleState, PluginKind, ResourcesSpec, SandboxCommand, SandboxError, SandboxExecutor,
    SandboxProcessSpec, SandboxProfile, UiTarget,
};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use crate::{read_ui_message_with_limits_and_gate, write_ui_message, UiFramingError};

/// Wall clock for a plugin's UI worker.
///
/// A UI worker is a *persistent* process — it lives as long as the surface
/// showing it — so this bounds a session, not a task. The short defaults that
/// suit a one-shot plugin script are a lifecycle bug here, not a tighter
/// policy: the worker is killed mid-use, the circuit opens, and it restarts
/// into the same fate.
const UI_WORKER_WALL_SECONDS: u64 = 86_400;

/// CPU seconds a UI worker gets when its manifest declared none.
///
/// The wall clock was promoted for a persistent worker and these two were not,
/// which left the lifecycle bug in place wearing different numbers. `30` is a
/// one-shot script's slice, enforced as a hard `RLIMIT_CPU` plus a watchdog —
/// a component that lives for a day reaches 30 CUMULATIVE seconds by simply
/// being used, and is then killed mid-use exactly as the 60-second wall clock
/// killed it. Matches the native-process arm, which has always set this
/// explicitly.
const UI_WORKER_CPU_SECONDS: u64 = 300;

/// Output megabytes a UI worker gets when its manifest declared none.
///
/// The 8 MiB default is a LIFETIME cap, not a rate: a worker streaming frames
/// for a day crosses it as a matter of course. Raised for the same reason as
/// the clock above — the bound should end a runaway, not a long session.
const UI_WORKER_OUTPUT_MB: u64 = 256;

/// The wall clock a `ui-component` worker runs under, given what its manifest
/// declared.
///
/// A `ui-component` manifest's resources really do describe this worker, so a
/// declared cap stands. The one it never declared does not: `ResourcesSpec` is
/// `#[serde(default)]`, so a manifest with no `[resources]` block silently
/// inherits the 60-second default meant for a short plugin script. A UI
/// component is not a script — it was killed at 60 seconds, tripped the
/// circuit, backed off, restarted, and did it again forever. The native-process
/// arm has always set a long clock here and said why; this arm inherited a
/// default nobody chose for it.
///
/// Equality with the default is how "the author said nothing" is detected,
/// which `#[serde(default)]` leaves no cleaner way to ask. An author who writes
/// exactly 60 also gets the long clock — the friendlier direction to be wrong
/// in, since the alternative is a UI that dies while someone is using it.
fn ui_worker_wall_seconds(declared: u64) -> u64 {
    if declared == ResourcesSpec::default().wall_seconds {
        UI_WORKER_WALL_SECONDS
    } else {
        declared
    }
}

/// The CPU and output caps a UI worker runs under, promoted on exactly the same
/// terms as the wall clock: an author who declared a value keeps it, one who
/// said nothing gets a persistent worker's budget rather than a script's.
///
/// Promoting the clock alone left the same kill/circuit-open loop in place — a
/// component was simply killed by `RLIMIT_CPU` or the output cap instead of by
/// the clock. All three defaults were written for a one-shot script; a worker
/// that lives as long as the surface showing it needs all three moved together.
fn ui_worker_cpu_seconds(declared: u64) -> u64 {
    if declared == ResourcesSpec::default().cpu_seconds {
        UI_WORKER_CPU_SECONDS
    } else {
        declared
    }
}

fn ui_worker_output_mb(declared: u64) -> u64 {
    if declared == ResourcesSpec::default().maximum_output_mb {
        UI_WORKER_OUTPUT_MB
    } else {
        declared
    }
}

/// Why a verified worker package is being started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiWorkerLaunchPurpose {
    /// Install-time handshake. A verified installed-disabled package may run,
    /// but it is stopped before the lifecycle can become enabled.
    SmokeTest,
    /// Normal user-facing execution. Only an enabled package may run.
    Active,
}

/// Host-owned JavaScript runtime and its read-only dependency root.
///
/// Production distributions should point this at Codypendent's bundled Node
/// directory. Dynamic development installs may use a package-manager prefix.
/// The component may read this trusted runtime tree but cannot write it, and
/// the executable must resolve within it.
#[derive(Debug, Clone)]
pub struct UiWorkerRuntime {
    executable: PathBuf,
    read_root: PathBuf,
}

impl UiWorkerRuntime {
    pub fn new(
        executable: impl AsRef<Path>,
        read_root: impl AsRef<Path>,
    ) -> Result<Self, UiWorkerError> {
        let executable = std::fs::canonicalize(executable.as_ref()).map_err(|source| {
            UiWorkerError::PackagePath {
                path: executable.as_ref().to_path_buf(),
                source,
            }
        })?;
        let read_root = std::fs::canonicalize(read_root.as_ref()).map_err(|source| {
            UiWorkerError::PackagePath {
                path: read_root.as_ref().to_path_buf(),
                source,
            }
        })?;
        if read_root.parent().is_none() || !read_root.is_dir() {
            return Err(UiWorkerError::InvalidRuntime(
                "runtime dependency root must be a non-root directory".into(),
            ));
        }
        if !executable.starts_with(&read_root) || !runtime_is_executable(&executable) {
            return Err(UiWorkerError::InvalidRuntime(
                "Node runtime must be an executable file within its trusted dependency root".into(),
            ));
        }
        validate_node_permission_runtime(&executable)?;
        Ok(Self {
            executable,
            read_root,
        })
    }

    /// Convenience for a self-contained runtime whose dependencies are beside
    /// the executable. Dynamically linked installations should use [`Self::new`]
    /// with their complete trusted runtime prefix.
    pub fn self_contained(executable: impl AsRef<Path>) -> Result<Self, UiWorkerError> {
        let executable = std::fs::canonicalize(executable.as_ref()).map_err(|source| {
            UiWorkerError::PackagePath {
                path: executable.as_ref().to_path_buf(),
                source,
            }
        })?;
        let read_root = executable.parent().ok_or_else(|| {
            UiWorkerError::InvalidRuntime("Node runtime has no dependency directory".into())
        })?;
        Self::new(&executable, read_root)
    }

    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    #[must_use]
    pub fn read_root(&self) -> &Path {
        &self.read_root
    }
}

fn validate_node_permission_runtime(executable: &Path) -> Result<(), UiWorkerError> {
    let output = std::process::Command::new(executable)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| {
            UiWorkerError::InvalidRuntime(format!("cannot query Node.js version: {error}"))
        })?;
    if !output.status.success() {
        return Err(UiWorkerError::InvalidRuntime(
            "host runtime did not report a Node.js version".into(),
        ));
    }
    let version = String::from_utf8_lossy(&output.stdout);
    let mut parts = version.trim().trim_start_matches('v').split('.');
    let major = parts.next().and_then(|part| part.parse::<u64>().ok());
    let minor = parts.next().and_then(|part| part.parse::<u64>().ok());
    if !matches!((major, minor), (Some(major), Some(minor)) if major > 22 || major == 22 && minor >= 13)
    {
        return Err(UiWorkerError::InvalidRuntime(format!(
            "UI workers require Node.js >=22.13 for the stable permission model; found {}",
            version.trim()
        )));
    }
    Ok(())
}

/// Immutable, verified launch input for one component worker.
#[derive(Debug, Clone)]
pub struct UiWorkerLaunch {
    plugin_id: String,
    publisher: String,
    signed: bool,
    target: UiTarget,
    entrypoint: PathBuf,
    profile: SandboxProfile,
    command: SandboxCommand,
    declared_capabilities: HashSet<String>,
    declared_contributions: HashMap<String, String>,
    verified_contributions: HashMap<String, VerifiedUiContribution>,
    package_seal: VerifiedPackageSeal,
    memory_limit_mb: u64,
    cpu_limit_seconds: u64,
}

/// Immutable contribution authority copied from the verified signed manifest.
/// Workers may choose document ids and presentation state, but may not rewrite
/// this renderer/target/fallback tuple on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedUiContribution {
    pub id: String,
    pub point: String,
    pub renderer: String,
    pub targets: Vec<UiTarget>,
    pub fallback_renderer: Option<String>,
    pub applicable_slot: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifiedPackageFile {
    relative_path: PathBuf,
    checksum: String,
    size: u64,
}

/// A launch-time seal over the exact regular-file tree extracted from the
/// signed `.cody-ui.tgz` artifact. It is checked once while resolving the
/// entrypoint and again immediately before spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifiedPackageSeal {
    root: PathBuf,
    artifact_checksum: String,
    files: Vec<VerifiedPackageFile>,
}

const MAX_UI_PACKAGE_FILES: usize = 10_000;
const MAX_UI_PACKAGE_ENTRIES: usize = 20_000;
const MAX_UI_PACKAGE_DIRECTORIES: usize = 10_000;
const MAX_UI_PACKAGE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_UI_PACKAGE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_UI_PACKAGE_ARCHIVE_BYTES: usize = 10 * 1024 * 1024;
const MAX_UI_PACKAGE_PATH_BYTES: usize = 4_096;
const MAX_UI_PACKAGE_PATH_DEPTH: usize = 64;

impl UiWorkerLaunch {
    /// Resolve the package entrypoint and bind it to an already verified plugin
    /// lifecycle record.
    ///
    /// `package_root` is the extracted, checksum-verified package directory;
    /// symlinks are resolved and the selected entrypoint must remain inside it.
    /// `node_executable` must be an absolute executable path supplied by the
    /// host installation, never a package-controlled command or `PATH` lookup.
    pub fn from_installed(
        installed: &InstalledPlugin,
        artifact: &[u8],
        package_root: impl AsRef<Path>,
        target: UiTarget,
        node_executable: impl AsRef<Path>,
        purpose: UiWorkerLaunchPurpose,
    ) -> Result<Self, UiWorkerError> {
        let runtime = UiWorkerRuntime::self_contained(node_executable)?;
        Self::from_installed_with_runtime(
            installed,
            artifact,
            package_root,
            target,
            runtime,
            purpose,
        )
    }

    /// Variant of [`Self::from_installed`] for dynamically linked Node
    /// distributions whose trusted runtime dependency root is larger than the
    /// executable directory.
    pub fn from_installed_with_runtime(
        installed: &InstalledPlugin,
        artifact: &[u8],
        package_root: impl AsRef<Path>,
        target: UiTarget,
        runtime: UiWorkerRuntime,
        purpose: UiWorkerLaunchPurpose,
    ) -> Result<Self, UiWorkerError> {
        if !matches!(
            installed.manifest().kind,
            PluginKind::UiComponent | PluginKind::NativeProcess
        ) || installed.manifest().ui.is_none()
        {
            return Err(UiWorkerError::NotUiComponent(
                installed.manifest().id.clone(),
            ));
        }
        match purpose {
            UiWorkerLaunchPurpose::Active if !installed.is_active() => {
                return Err(UiWorkerError::InactivePlugin {
                    plugin: installed.manifest().id.clone(),
                    state: installed.state(),
                });
            }
            UiWorkerLaunchPurpose::SmokeTest
                if !matches!(
                    installed.state(),
                    LifecycleState::InstalledDisabled | LifecycleState::SmokeTested
                ) =>
            {
                return Err(UiWorkerError::InactivePlugin {
                    plugin: installed.manifest().id.clone(),
                    state: installed.state(),
                });
            }
            _ => {}
        }
        if installed.content_hash().trim() != installed.manifest().security.checksum.trim()
            || !valid_sha256(installed.content_hash())
        {
            return Err(UiWorkerError::UnverifiedPackage(
                installed.manifest().id.clone(),
            ));
        }

        let ui = installed
            .manifest()
            .ui
            .as_ref()
            .ok_or(UiWorkerError::MissingEntrypoint(target))?;
        let relative = match target {
            UiTarget::Shared => ui.entrypoints.shared.as_ref(),
            UiTarget::Terminal => ui
                .entrypoints
                .terminal
                .as_ref()
                .or(ui.entrypoints.shared.as_ref()),
            UiTarget::Web => ui
                .entrypoints
                .web
                .as_ref()
                .or(ui.entrypoints.shared.as_ref()),
        }
        .ok_or(UiWorkerError::MissingEntrypoint(target))?;

        let package_seal = verify_extracted_package(installed, artifact, package_root.as_ref())?;
        let package_root = package_seal.root.clone();
        if !package_root.is_dir() {
            return Err(UiWorkerError::InvalidEntrypoint(
                "package root is not a directory".into(),
            ));
        }
        let entrypoint_candidate = package_root.join(relative);
        let entrypoint = std::fs::canonicalize(&entrypoint_candidate).map_err(|source| {
            UiWorkerError::PackagePath {
                path: entrypoint_candidate,
                source,
            }
        })?;
        if !entrypoint.starts_with(&package_root) || !entrypoint.is_file() {
            return Err(UiWorkerError::InvalidEntrypoint(
                "entrypoint escapes its verified package root or is not a file".into(),
            ));
        }
        match entrypoint
            .extension()
            .and_then(|extension| extension.to_str())
        {
            Some("js" | "mjs" | "cjs") => {}
            _ => {
                return Err(UiWorkerError::InvalidEntrypoint(
                    "entrypoint must be precompiled .js, .mjs, or .cjs; TypeScript is never evaluated in-process"
                        .into(),
                ));
            }
        }

        // A native integration and its UI share one signed artifact/lifecycle,
        // but they emphatically do not share process authority. The UI side is
        // a separate, networkless worker with no native filesystem, secret, or
        // subprocess grants. Standalone `ui-component` manifests already have
        // no native grants and retain their explicitly declared resource caps.
        let mut profile = if installed.manifest().kind == PluginKind::NativeProcess {
            let mut profile =
                SandboxProfile::derive(installed.manifest(), &CapabilitySet::default());
            profile.memory_mb = 128;
            profile.cpu_seconds = 300;
            profile.wall_seconds = UI_WORKER_WALL_SECONDS;
            profile.maximum_output_mb = 8;
            profile
        } else {
            let mut profile = SandboxProfile::derive(installed.manifest(), installed.granted());
            profile.wall_seconds = ui_worker_wall_seconds(profile.wall_seconds);
            profile.cpu_seconds = ui_worker_cpu_seconds(profile.cpu_seconds);
            profile.maximum_output_mb = ui_worker_output_mb(profile.maximum_output_mb);
            profile
        };
        let root_string = package_root.to_string_lossy().into_owned();
        if !profile.read_paths.contains(&root_string) {
            profile.read_paths.push(root_string);
        }
        let runtime_root = runtime.read_root.to_string_lossy().into_owned();
        if !profile.read_paths.contains(&runtime_root) {
            profile.read_paths.push(runtime_root);
        }
        let maximum_old_space_mb = profile
            .memory_mb
            .saturating_mul(3)
            .saturating_div(4)
            .max(16);
        let command = SandboxCommand::node_permission_runtime(
            runtime.executable,
            vec![
                "--permission".into(),
                format!("--allow-fs-read={}", package_root.to_string_lossy()),
                format!("--max-old-space-size={maximum_old_space_mb}"),
                "--unhandled-rejections=strict".into(),
                "--disable-proto=delete".into(),
                "--no-addons".into(),
                entrypoint.to_string_lossy().into_owned(),
            ],
            &package_root,
            format!("plugin-ui:{}", installed.manifest().id),
        )?;
        let declared_capabilities = installed
            .granted_ui_capabilities()
            .iter()
            .map(|capability| capability.as_str().to_string())
            .collect();
        let declared_contributions = ui
            .contributions
            .iter()
            .map(|contribution| {
                (
                    contribution.id.clone(),
                    contribution.point.as_str().to_string(),
                )
            })
            .collect();
        let verified_contributions = ui
            .contributions
            .iter()
            .map(|contribution| {
                let point = contribution.point.as_str().to_owned();
                (
                    contribution.id.clone(),
                    VerifiedUiContribution {
                        id: contribution.id.clone(),
                        point: point.clone(),
                        renderer: contribution.renderer.clone(),
                        targets: contribution.targets.clone(),
                        fallback_renderer: contribution.fallback_renderer.clone(),
                        applicable_slot: point,
                    },
                )
            })
            .collect();

        let memory_limit_mb = profile.memory_mb;
        let cpu_limit_seconds = profile.cpu_seconds;
        Ok(Self {
            plugin_id: installed.manifest().id.clone(),
            publisher: installed.manifest().publisher.clone(),
            signed: installed.manifest().security.is_signed(),
            target,
            entrypoint,
            profile,
            command,
            declared_capabilities,
            declared_contributions,
            verified_contributions,
            package_seal,
            memory_limit_mb,
            cpu_limit_seconds,
        })
    }

    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    #[must_use]
    pub fn publisher(&self) -> &str {
        &self.publisher
    }

    #[must_use]
    pub fn is_signed(&self) -> bool {
        self.signed
    }

    #[must_use]
    pub fn target(&self) -> UiTarget {
        self.target
    }

    /// Verified immutable package generation used for instance-scoped worker
    /// health and circuit identity.
    #[must_use]
    pub fn generation_key(&self) -> &str {
        &self.package_seal.artifact_checksum
    }

    #[must_use]
    pub fn entrypoint(&self) -> &Path {
        &self.entrypoint
    }

    /// Capabilities bound to the verified manifest and narrowed grant set.
    #[must_use]
    pub fn declared_capabilities(&self) -> &HashSet<String> {
        &self.declared_capabilities
    }

    /// Contribution id to point mappings bound to the verified manifest.
    #[must_use]
    pub fn declared_contributions(&self) -> &HashMap<String, String> {
        &self.declared_contributions
    }

    #[must_use]
    pub fn verified_contributions(&self) -> &HashMap<String, VerifiedUiContribution> {
        &self.verified_contributions
    }

    /// Host-attested memory reservation used by daemon aggregate worker
    /// admission control before the process is spawned.
    #[must_use]
    pub fn memory_limit_mb(&self) -> u64 {
        self.memory_limit_mb
    }

    fn revalidate_package(&self) -> Result<(), UiWorkerError> {
        revalidate_package_seal(&self.package_seal)
    }
}

fn package_error(message: impl Into<String>) -> UiWorkerError {
    UiWorkerError::PackageVerification(message.into())
}

fn normalized_archive_path(path: &Path) -> Result<PathBuf, UiWorkerError> {
    use std::path::Component;

    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(package_error("archive contains an empty or absolute path"));
    }
    if path.as_os_str().as_encoded_bytes().len() > MAX_UI_PACKAGE_PATH_BYTES
        || path.components().count() > MAX_UI_PACKAGE_PATH_DEPTH
    {
        return Err(package_error(
            "archive path exceeds host length/depth limits",
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            _ => {
                return Err(package_error(
                    "archive path is not normalized or attempts to escape the package root",
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(package_error("archive contains an empty path"));
    }
    Ok(normalized)
}

fn verify_extracted_package(
    installed: &InstalledPlugin,
    artifact: &[u8],
    package_root: &Path,
) -> Result<VerifiedPackageSeal, UiWorkerError> {
    if artifact.len() > MAX_UI_PACKAGE_ARCHIVE_BYTES {
        return Err(package_error("compressed package exceeds the host limit"));
    }
    let artifact_checksum = checksum_of(artifact);
    if artifact_checksum != installed.content_hash()
        || artifact_checksum != installed.manifest().security.checksum.trim()
    {
        return Err(UiWorkerError::UnverifiedPackage(
            installed.manifest().id.clone(),
        ));
    }
    if std::fs::symlink_metadata(package_root)
        .map_err(|source| UiWorkerError::PackagePath {
            path: package_root.to_path_buf(),
            source,
        })?
        .file_type()
        .is_symlink()
    {
        return Err(package_error("package root must not be a symbolic link"));
    }
    let root =
        std::fs::canonicalize(package_root).map_err(|source| UiWorkerError::PackagePath {
            path: package_root.to_path_buf(),
            source,
        })?;
    if !root.is_dir() {
        return Err(package_error("package root is not a directory"));
    }

    let decoder = flate2::read::GzDecoder::new(Cursor::new(artifact));
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| package_error(format!("invalid gzip/ustar package: {error}")))?;
    let mut archive_files = BTreeMap::<PathBuf, VerifiedPackageFile>::new();
    let mut total_bytes = 0_u64;
    let mut total_entries = 0_usize;
    let mut total_directories = 0_usize;
    for entry in entries {
        let mut entry =
            entry.map_err(|error| package_error(format!("invalid ustar entry: {error}")))?;
        total_entries += 1;
        if total_entries > MAX_UI_PACKAGE_ENTRIES {
            return Err(package_error("package exceeds the total-entry limit"));
        }
        let relative = normalized_archive_path(
            &entry
                .path()
                .map_err(|error| package_error(format!("invalid archive path: {error}")))?,
        )?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            total_directories += 1;
            if total_directories > MAX_UI_PACKAGE_DIRECTORIES {
                return Err(package_error("package exceeds the directory-entry limit"));
            }
            continue;
        }
        if !entry_type.is_file() {
            return Err(package_error(format!(
                "archive entry `{}` is not a regular file",
                relative.display()
            )));
        }
        if archive_files.len() >= MAX_UI_PACKAGE_FILES {
            return Err(package_error("package exceeds the regular-file limit"));
        }
        let declared_size = entry.size();
        if declared_size > MAX_UI_PACKAGE_FILE_BYTES {
            return Err(package_error(format!(
                "archive entry `{}` exceeds the per-file size limit",
                relative.display()
            )));
        }
        total_bytes = total_bytes
            .checked_add(declared_size)
            .ok_or_else(|| package_error("package size overflow"))?;
        if total_bytes > MAX_UI_PACKAGE_BYTES {
            return Err(package_error("package exceeds the uncompressed size limit"));
        }
        let mut content = Vec::with_capacity(usize::try_from(declared_size).unwrap_or(0));
        entry
            .by_ref()
            .take(MAX_UI_PACKAGE_FILE_BYTES + 1)
            .read_to_end(&mut content)
            .map_err(|error| package_error(format!("cannot read archive entry: {error}")))?;
        if content.len() as u64 != declared_size {
            return Err(package_error(format!(
                "archive entry `{}` has a mismatched size",
                relative.display()
            )));
        }
        let file = VerifiedPackageFile {
            relative_path: relative.clone(),
            checksum: checksum_of(&content),
            size: declared_size,
        };
        if archive_files.insert(relative, file).is_some() {
            return Err(package_error(
                "archive contains a duplicate regular-file path",
            ));
        }
    }
    if archive_files.is_empty() {
        return Err(package_error("package contains no regular files"));
    }

    let seal = VerifiedPackageSeal {
        root,
        artifact_checksum,
        files: archive_files.into_values().collect(),
    };
    revalidate_package_seal(&seal)?;
    Ok(seal)
}

fn revalidate_package_seal(seal: &VerifiedPackageSeal) -> Result<(), UiWorkerError> {
    let actual_paths = collect_regular_files(&seal.root)?;
    let expected_paths = seal
        .files
        .iter()
        .map(|file| file.relative_path.clone())
        .collect::<BTreeSet<_>>();
    if actual_paths != expected_paths {
        return Err(package_error(
            "extracted package file set differs from the verified artifact",
        ));
    }
    for expected in &seal.files {
        let path = seal.root.join(&expected.relative_path);
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|source| UiWorkerError::PackagePath {
                path: path.clone(),
                source,
            })?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() != expected.size
        {
            return Err(package_error(format!(
                "extracted entry `{}` is not the verified regular file",
                expected.relative_path.display()
            )));
        }
        let canonical =
            std::fs::canonicalize(&path).map_err(|source| UiWorkerError::PackagePath {
                path: path.clone(),
                source,
            })?;
        if !canonical.starts_with(&seal.root) {
            return Err(package_error("extracted file escapes the package root"));
        }
        let content = std::fs::read(&path).map_err(|source| UiWorkerError::PackagePath {
            path: path.clone(),
            source,
        })?;
        if checksum_of(&content) != expected.checksum {
            return Err(package_error(format!(
                "extracted entry `{}` differs from the verified artifact",
                expected.relative_path.display()
            )));
        }
    }
    Ok(())
}

fn collect_regular_files(root: &Path) -> Result<BTreeSet<PathBuf>, UiWorkerError> {
    fn visit(
        root: &Path,
        directory: &Path,
        output: &mut BTreeSet<PathBuf>,
    ) -> Result<(), UiWorkerError> {
        for entry in std::fs::read_dir(directory).map_err(|source| UiWorkerError::PackagePath {
            path: directory.to_path_buf(),
            source,
        })? {
            let entry = entry.map_err(|source| UiWorkerError::PackagePath {
                path: directory.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            let metadata =
                std::fs::symlink_metadata(&path).map_err(|source| UiWorkerError::PackagePath {
                    path: path.clone(),
                    source,
                })?;
            if metadata.file_type().is_symlink() {
                return Err(package_error("extracted package contains a symbolic link"));
            }
            if metadata.is_dir() {
                visit(root, &path, output)?;
            } else if metadata.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| package_error("extracted file escapes package root"))?
                    .to_path_buf();
                if output.len() >= MAX_UI_PACKAGE_FILES {
                    return Err(package_error("extracted package exceeds the file limit"));
                }
                output.insert(relative);
            } else {
                return Err(package_error(
                    "extracted package contains a non-regular filesystem entry",
                ));
            }
        }
        Ok(())
    }

    let mut output = BTreeSet::new();
    visit(root, root, &mut output)?;
    Ok(output)
}

fn valid_sha256(value: &str) -> bool {
    let Some(digest) = value.trim().strip_prefix("sha256:") else {
        return false;
    };
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn runtime_is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Runtime, transport, heartbeat, and restart ceilings for workers.
#[derive(Debug, Clone)]
pub struct UiWorkerConfig {
    pub hard_limits: UiHardLimits,
    pub ready_timeout: Duration,
    pub heartbeat_interval: Duration,
    pub heartbeat_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub maximum_lifetime: Duration,
    pub maximum_messages: u64,
    pub message_rate_per_second: u32,
    pub message_rate_burst: u32,
    pub byte_rate_per_second: u64,
    pub byte_rate_burst: u64,
    pub stderr_bytes: usize,
    pub circuit_failure_threshold: u32,
    pub circuit_window: Duration,
    pub circuit_cooldown: Duration,
    pub initial_restart_backoff: Duration,
    pub maximum_restart_backoff: Duration,
}

impl Default for UiWorkerConfig {
    fn default() -> Self {
        Self {
            hard_limits: UiHardLimits::default(),
            ready_timeout: Duration::from_secs(10),
            heartbeat_interval: Duration::from_secs(15),
            heartbeat_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(3),
            maximum_lifetime: Duration::from_secs(60 * 60),
            maximum_messages: 1_000_000,
            message_rate_per_second: UI_WORKER_MESSAGE_RATE_PER_SECOND,
            message_rate_burst: UI_WORKER_MESSAGE_BURST,
            byte_rate_per_second: 4 * 1024 * 1024,
            byte_rate_burst: 16 * 1024 * 1024,
            stderr_bytes: 256 * 1024,
            circuit_failure_threshold: 5,
            circuit_window: Duration::from_secs(60),
            circuit_cooldown: Duration::from_secs(30),
            initial_restart_backoff: Duration::from_millis(250),
            maximum_restart_backoff: Duration::from_secs(30),
        }
    }
}

impl UiWorkerConfig {
    fn validate(&self) -> Result<(), UiWorkerError> {
        self.hard_limits
            .validate()
            .map_err(|error| UiWorkerError::InvalidConfiguration(error.to_string()))?;
        for (name, duration) in [
            ("ready_timeout", self.ready_timeout),
            ("heartbeat_interval", self.heartbeat_interval),
            ("heartbeat_timeout", self.heartbeat_timeout),
            ("shutdown_timeout", self.shutdown_timeout),
            ("maximum_lifetime", self.maximum_lifetime),
            ("circuit_window", self.circuit_window),
            ("circuit_cooldown", self.circuit_cooldown),
            ("initial_restart_backoff", self.initial_restart_backoff),
            ("maximum_restart_backoff", self.maximum_restart_backoff),
        ] {
            if duration.is_zero() {
                return Err(UiWorkerError::InvalidConfiguration(format!(
                    "{name} must be greater than zero"
                )));
            }
        }
        for (name, value) in [
            ("maximum_messages", self.maximum_messages),
            (
                "message_rate_per_second",
                u64::from(self.message_rate_per_second),
            ),
            ("byte_rate_per_second", self.byte_rate_per_second),
            ("byte_rate_burst", self.byte_rate_burst),
            (
                "circuit_failure_threshold",
                u64::from(self.circuit_failure_threshold),
            ),
            ("stderr_bytes", self.stderr_bytes as u64),
        ] {
            if value == 0 {
                return Err(UiWorkerError::InvalidConfiguration(format!(
                    "{name} must be greater than zero"
                )));
            }
        }
        if self.maximum_restart_backoff < self.initial_restart_backoff {
            return Err(UiWorkerError::InvalidConfiguration(
                "maximum_restart_backoff must not be less than initial_restart_backoff".into(),
            ));
        }
        Ok(())
    }
}

/// Sanitized and redacted worker stderr retained for operator diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UiWorkerDiagnostics {
    pub text: String,
    pub truncated: bool,
    pub stripped_controls: usize,
}

/// Signals consumed by the trusted host session.
#[derive(Debug, Clone, PartialEq)]
pub enum UiWorkerSignal {
    Message(Box<UiWireMessage>),
    Heartbeat,
    ResyncRequested {
        document_id: Option<UiDocumentId>,
        revision: Option<UiRevision>,
        reason: Option<String>,
    },
    Reloaded,
}

#[derive(Debug, thiserror::Error)]
pub enum UiWorkerError {
    #[error("invalid UI worker configuration: {0}")]
    InvalidConfiguration(String),
    #[error("plugin `{0}` does not declare a governed UI surface")]
    NotUiComponent(String),
    #[error("plugin `{plugin}` cannot start a UI worker while in state {state:?}")]
    InactivePlugin {
        plugin: String,
        state: LifecycleState,
    },
    #[error("plugin `{0}` has no trustworthy verification record")]
    UnverifiedPackage(String),
    #[error("UI package verification failed: {0}")]
    PackageVerification(String),
    #[error("plugin has no entrypoint for UI target {0:?}")]
    MissingEntrypoint(UiTarget),
    #[error("invalid UI worker entrypoint: {0}")]
    InvalidEntrypoint(String),
    #[error("invalid UI worker runtime: {0}")]
    InvalidRuntime(String),
    #[error("the trusted codypendent UI worker resource launcher is unavailable")]
    ResourceLauncherUnavailable,
    #[error("cannot resolve package path `{path}`: {source}")]
    PackagePath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
    #[error("spawning sandboxed UI worker failed: {0}")]
    Spawn(#[source] std::io::Error),
    #[error(transparent)]
    Framing(#[from] UiFramingError),
    #[error("UI worker did not become ready within {0:?}")]
    ReadinessTimeout(Duration),
    #[error("UI worker handshake failed: {0}")]
    Handshake(String),
    #[error("UI worker startup failed: {source}; sanitized stderr: {diagnostics}")]
    Startup {
        #[source]
        source: Box<UiWorkerError>,
        diagnostics: String,
    },
    #[error("UI worker exited before completing the handshake")]
    ExitedDuringHandshake,
    #[error("UI worker heartbeat timed out after {0:?}")]
    HeartbeatTimeout(Duration),
    #[error("UI worker exceeded its {resource} limit of {limit}")]
    ResourceLimitExceeded {
        resource: &'static str,
        limit: String,
    },
    #[error("UI worker exceeded its maximum lifetime of {0:?}")]
    LifetimeExceeded(Duration),
    #[error("UI worker exceeded its message budget")]
    MessageBudgetExceeded,
    #[error("UI worker exceeded its message rate")]
    MessageRateExceeded,
    #[error("UI worker emitted a disallowed message: {0}")]
    DisallowedMessage(String),
    #[error("UI worker transport reached EOF")]
    UnexpectedEof,
    #[error("UI worker circuit for `{plugin}` is open for another {remaining:?}")]
    CircuitOpen { plugin: String, remaining: Duration },
    #[error("UI worker `{plugin}` is in restart backoff for another {remaining:?}")]
    RestartBackoff { plugin: String, remaining: Duration },
    #[error("UI worker state lock was poisoned")]
    StatePoisoned,
    #[error("UI worker aggregate admission denied by the host {resource} limit of {limit}")]
    AggregateAdmissionDenied { resource: &'static str, limit: u64 },
}

#[derive(Debug, Default)]
struct CircuitState {
    failures: VecDeque<Instant>,
    consecutive_failures: u32,
    next_launch: Option<Instant>,
    open_until: Option<Instant>,
}

/// Observable restart protection for one plugin worker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UiWorkerCircuitStatus {
    pub recent_failures: u32,
    pub restart_backoff_remaining: Option<Duration>,
    pub circuit_open_remaining: Option<Duration>,
}

/// Factory and circuit breaker for verified UI workers.
pub struct UiWorkerSupervisor {
    executor: Arc<dyn SandboxExecutor>,
    resource_launcher: Option<PathBuf>,
    config: UiWorkerConfig,
    circuits: Arc<Mutex<HashMap<String, CircuitState>>>,
    admission: Arc<Mutex<UiWorkerAdmissionState>>,
}

const MAX_SUPERVISED_UI_WORKERS: u64 = 64;
const MAX_SUPERVISED_UI_MEMORY_MB: u64 = 8 * 1024;

#[derive(Debug, Default)]
struct UiWorkerAdmissionState {
    workers: u64,
    memory_mb: u64,
}

#[derive(Debug)]
struct UiWorkerAdmission {
    state: Arc<Mutex<UiWorkerAdmissionState>>,
    memory_mb: u64,
}

impl Drop for UiWorkerAdmission {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.workers = state.workers.saturating_sub(1);
            state.memory_mb = state.memory_mb.saturating_sub(self.memory_mb);
        }
    }
}

impl UiWorkerSupervisor {
    pub fn new(
        executor: Arc<dyn SandboxExecutor>,
        config: UiWorkerConfig,
    ) -> Result<Self, UiWorkerError> {
        config.validate()?;
        Ok(Self {
            executor,
            resource_launcher: None,
            config,
            circuits: Arc::new(Mutex::new(HashMap::new())),
            admission: Arc::new(Mutex::new(UiWorkerAdmissionState::default())),
        })
    }

    /// Construct a supervisor backed by the platform's enforcing sandbox. This
    /// fails closed where Seatbelt/bubblewrap is unavailable.
    pub fn system(config: UiWorkerConfig) -> Result<Self, UiWorkerError> {
        let executor: Arc<dyn SandboxExecutor> = Arc::from(enforcing_executor()?);
        let launcher =
            locate_resource_launcher().ok_or(UiWorkerError::ResourceLauncherUnavailable)?;
        Self::with_resource_launcher(executor, config, launcher)
    }

    /// Construct with an explicit, host-installed pre-exec resource launcher.
    /// The path is resolved before any untrusted package is involved.
    pub fn with_resource_launcher(
        executor: Arc<dyn SandboxExecutor>,
        config: UiWorkerConfig,
        launcher: impl AsRef<Path>,
    ) -> Result<Self, UiWorkerError> {
        config.validate()?;
        let launcher = std::fs::canonicalize(launcher.as_ref()).map_err(|source| {
            UiWorkerError::PackagePath {
                path: launcher.as_ref().to_path_buf(),
                source,
            }
        })?;
        if !trusted_launcher_file(&launcher) {
            return Err(UiWorkerError::ResourceLauncherUnavailable);
        }
        Ok(Self {
            executor,
            resource_launcher: Some(launcher),
            config,
            circuits: Arc::new(Mutex::new(HashMap::new())),
            admission: Arc::new(Mutex::new(UiWorkerAdmissionState::default())),
        })
    }

    /// Start, negotiate, and await readiness from one worker.
    pub async fn launch(
        &self,
        launch: UiWorkerLaunch,
        host_capabilities: UiCapabilities,
    ) -> Result<UiWorker, UiWorkerError> {
        let circuit_key = launch.plugin_id.clone();
        self.launch_instance(launch, host_capabilities, circuit_key)
            .await
    }

    /// Launch under a host-attested instance identity (for example
    /// plugin+session+target+generation) so one worker cannot open or clear an
    /// unrelated instance's circuit.
    pub async fn launch_instance(
        &self,
        launch: UiWorkerLaunch,
        host_capabilities: UiCapabilities,
        circuit_key: String,
    ) -> Result<UiWorker, UiWorkerError> {
        self.check_circuit_key(&circuit_key, &launch.plugin_id)?;
        // Narrow the verification→exec window: the artifact-bound file set and
        // every file digest are checked again immediately before sandbox setup.
        launch.revalidate_package()?;
        let admission = self.try_admit(launch.memory_limit_mb)?;
        let spec = self
            .executor
            .prepare_interactive(&launch.profile, &launch.command)?;
        let child = spawn_sandboxed(
            &spec,
            self.resource_launcher.as_deref(),
            launch.memory_limit_mb,
            launch.cpu_limit_seconds,
        )?;
        let health = WorkerHealth {
            circuit_key,
            config: self.config.clone(),
            circuits: Arc::clone(&self.circuits),
        };
        let mut worker =
            UiWorker::from_child(child, spec, launch, self.config.clone(), health, admission)?;
        if let Err(error) = worker.handshake(host_capabilities).await {
            worker.record_failure();
            worker.terminate().await;
            let diagnostics = worker.diagnostics().text;
            return Err(UiWorkerError::Startup {
                source: Box::new(error),
                diagnostics,
            });
        }
        Ok(worker)
    }

    /// Restart after the currently scheduled exponential backoff. An open
    /// circuit is still returned immediately so an operator must wait for the
    /// cooldown or explicitly reset it.
    pub async fn restart(
        &self,
        launch: UiWorkerLaunch,
        host_capabilities: UiCapabilities,
    ) -> Result<UiWorker, UiWorkerError> {
        match self.check_circuit(&launch.plugin_id) {
            Err(UiWorkerError::RestartBackoff { remaining, .. }) => sleep(remaining).await,
            Err(error) => return Err(error),
            Ok(()) => {}
        }
        self.launch(launch, host_capabilities).await
    }

    pub async fn restart_instance(
        &self,
        launch: UiWorkerLaunch,
        host_capabilities: UiCapabilities,
        circuit_key: String,
    ) -> Result<UiWorker, UiWorkerError> {
        match self.check_circuit_key(&circuit_key, &launch.plugin_id) {
            Err(UiWorkerError::RestartBackoff { remaining, .. }) => sleep(remaining).await,
            Err(error) => return Err(error),
            Ok(()) => {}
        }
        self.launch_instance(launch, host_capabilities, circuit_key)
            .await
    }

    pub fn circuit_status(&self, plugin_id: &str) -> Result<UiWorkerCircuitStatus, UiWorkerError> {
        let now = Instant::now();
        let circuits = self
            .circuits
            .lock()
            .map_err(|_| UiWorkerError::StatePoisoned)?;
        let Some(state) = circuits.get(plugin_id) else {
            return Ok(UiWorkerCircuitStatus::default());
        };
        Ok(UiWorkerCircuitStatus {
            recent_failures: u32::try_from(state.failures.len()).unwrap_or(u32::MAX),
            restart_backoff_remaining: state
                .next_launch
                .filter(|deadline| *deadline > now)
                .map(|deadline| deadline.saturating_duration_since(now)),
            circuit_open_remaining: state
                .open_until
                .filter(|deadline| *deadline > now)
                .map(|deadline| deadline.saturating_duration_since(now)),
        })
    }

    /// Operator/developer reset after the underlying package or configuration
    /// was changed. Routine callers should let successful shutdown reset health.
    pub fn reset_circuit(&self, plugin_id: &str) -> Result<(), UiWorkerError> {
        self.circuits
            .lock()
            .map_err(|_| UiWorkerError::StatePoisoned)?
            .remove(plugin_id);
        Ok(())
    }

    fn check_circuit(&self, plugin_id: &str) -> Result<(), UiWorkerError> {
        self.check_circuit_key(plugin_id, plugin_id)
    }

    fn check_circuit_key(&self, circuit_key: &str, plugin_id: &str) -> Result<(), UiWorkerError> {
        let now = Instant::now();
        let circuits = self
            .circuits
            .lock()
            .map_err(|_| UiWorkerError::StatePoisoned)?;
        let Some(state) = circuits.get(circuit_key) else {
            return Ok(());
        };
        if let Some(open_until) = state.open_until.filter(|until| *until > now) {
            return Err(UiWorkerError::CircuitOpen {
                plugin: plugin_id.to_string(),
                remaining: open_until.saturating_duration_since(now),
            });
        }
        if let Some(next_launch) = state.next_launch.filter(|until| *until > now) {
            return Err(UiWorkerError::RestartBackoff {
                plugin: plugin_id.to_string(),
                remaining: next_launch.saturating_duration_since(now),
            });
        }
        Ok(())
    }

    fn try_admit(&self, memory_mb: u64) -> Result<UiWorkerAdmission, UiWorkerError> {
        let mut state = self
            .admission
            .lock()
            .map_err(|_| UiWorkerError::StatePoisoned)?;
        if state.workers >= MAX_SUPERVISED_UI_WORKERS {
            return Err(UiWorkerError::AggregateAdmissionDenied {
                resource: "worker count",
                limit: MAX_SUPERVISED_UI_WORKERS,
            });
        }
        if state.memory_mb.saturating_add(memory_mb) > MAX_SUPERVISED_UI_MEMORY_MB {
            return Err(UiWorkerError::AggregateAdmissionDenied {
                resource: "declared memory",
                limit: MAX_SUPERVISED_UI_MEMORY_MB,
            });
        }
        state.workers += 1;
        state.memory_mb += memory_mb;
        drop(state);
        Ok(UiWorkerAdmission {
            state: Arc::clone(&self.admission),
            memory_mb,
        })
    }
}

fn locate_resource_launcher() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = option_env!("CARGO_BIN_EXE_codypendent-ui-worker-launcher") {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(current) = std::env::current_exe() {
        if let Some(directory) = current.parent() {
            candidates.push(directory.join("codypendent-ui-worker-launcher"));
            if let Some(parent) = directory.parent() {
                candidates.push(parent.join("codypendent-ui-worker-launcher"));
            }
        }
    }
    let workspace_debug = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/codypendent-ui-worker-launcher");
    candidates.push(workspace_debug);
    candidates
        .into_iter()
        .filter_map(|candidate| std::fs::canonicalize(candidate).ok())
        .find(|candidate| trusted_launcher_file(candidate))
}

fn trusted_launcher_file(path: &Path) -> bool {
    if !runtime_is_executable(path) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let Ok(metadata) = std::fs::metadata(path) else {
            return false;
        };
        let expected_owner = std::env::current_exe()
            .ok()
            .and_then(|current| std::fs::metadata(current).ok())
            .map(|current| current.uid())
            .unwrap_or(metadata.uid());
        if metadata.permissions().mode() & 0o022 != 0
            || (metadata.uid() != 0 && metadata.uid() != expected_owner)
        {
            return false;
        }
        // A protected file below an attacker-writable directory is still
        // replaceable by rename. Validate every ancestor through the root.
        path.parent()
            .into_iter()
            .flat_map(Path::ancestors)
            .all(|ancestor| {
                std::fs::metadata(ancestor).is_ok_and(|metadata| {
                    metadata.permissions().mode() & 0o022 == 0
                        && (metadata.uid() == 0 || metadata.uid() == expected_owner)
                })
            })
    }
    #[cfg(not(unix))]
    true
}

fn spawn_sandboxed(
    spec: &SandboxProcessSpec,
    resource_launcher: Option<&Path>,
    memory_limit_mb: u64,
    cpu_limit_seconds: u64,
) -> Result<Child, UiWorkerError> {
    let Some((program, arguments)) = spec.argv().split_first() else {
        return Err(UiWorkerError::InvalidRuntime(
            "sandbox returned an empty argv".into(),
        ));
    };
    let resource_launcher = resource_launcher.ok_or(UiWorkerError::ResourceLauncherUnavailable)?;
    let memory_bytes = memory_limit_mb.saturating_mul(1024 * 1024);
    let mut command = Command::new(resource_launcher);
    command
        .arg("--memory-bytes")
        .arg(memory_bytes.to_string())
        .arg("--cpu-seconds")
        .arg(cpu_limit_seconds.to_string())
        .arg("--")
        .arg(program)
        .args(arguments)
        .current_dir(spec.cwd())
        .env_clear()
        .envs(spec.environment().iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    command.spawn().map_err(UiWorkerError::Spawn)
}

#[derive(Clone)]
struct WorkerHealth {
    circuit_key: String,
    config: UiWorkerConfig,
    circuits: Arc<Mutex<HashMap<String, CircuitState>>>,
}

impl WorkerHealth {
    fn failure(&self) {
        let Ok(mut circuits) = self.circuits.lock() else {
            return;
        };
        let now = Instant::now();
        let state = circuits.entry(self.circuit_key.clone()).or_default();
        while state
            .failures
            .front()
            .is_some_and(|failure| now.duration_since(*failure) > self.config.circuit_window)
        {
            state.failures.pop_front();
        }
        state.failures.push_back(now);
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        let exponent = state.consecutive_failures.saturating_sub(1).min(20);
        let multiplier = 1_u32 << exponent;
        let backoff = self
            .config
            .initial_restart_backoff
            .saturating_mul(multiplier)
            .min(self.config.maximum_restart_backoff);
        state.next_launch = Some(now + backoff);
        if state.failures.len() >= self.config.circuit_failure_threshold as usize {
            state.open_until = Some(now + self.config.circuit_cooldown);
        }
    }

    fn success(&self) {
        let Ok(mut circuits) = self.circuits.lock() else {
            return;
        };
        circuits.remove(&self.circuit_key);
    }
}

struct DiagnosticBuffer {
    bytes: Vec<u8>,
    maximum: usize,
    truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceViolation {
    Memory {
        limit_mb: u64,
    },
    Cpu {
        limit_seconds: u64,
    },
    Output {
        limit_bytes: u64,
    },
    OutputRate {
        bytes_per_second: u64,
        burst_bytes: u64,
    },
    AccountingUnavailable,
}

impl ResourceViolation {
    fn into_error(self) -> UiWorkerError {
        match self {
            Self::Memory { limit_mb } => UiWorkerError::ResourceLimitExceeded {
                resource: "memory",
                limit: format!("{limit_mb} MiB"),
            },
            Self::Cpu { limit_seconds } => UiWorkerError::ResourceLimitExceeded {
                resource: "CPU time",
                limit: format!("{limit_seconds} seconds"),
            },
            Self::Output { limit_bytes } => UiWorkerError::ResourceLimitExceeded {
                resource: "worker output",
                limit: format!("{} bytes", limit_bytes),
            },
            Self::OutputRate {
                bytes_per_second,
                burst_bytes,
            } => UiWorkerError::ResourceLimitExceeded {
                resource: "worker stderr rate",
                limit: format!("{bytes_per_second} bytes/second with {burst_bytes} bytes burst"),
            },
            Self::AccountingUnavailable => UiWorkerError::ResourceLimitExceeded {
                resource: "resource accounting",
                limit: "available watchdog".into(),
            },
        }
    }
}

impl DiagnosticBuffer {
    fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(maximum.min(16 * 1024)),
            maximum,
            truncated: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let available = self.maximum.saturating_sub(self.bytes.len());
        let keep = available.min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..keep]);
        self.truncated |= keep < bytes.len();
    }

    fn snapshot(&self, origin: &str) -> UiWorkerDiagnostics {
        let sanitized = sanitize_untrusted(
            format!("{origin} (stderr)"),
            &String::from_utf8_lossy(&self.bytes),
            self.maximum,
        );
        UiWorkerDiagnostics {
            text: redact_sensitive_values(&sanitized.text),
            truncated: self.truncated || sanitized.truncated,
            stripped_controls: sanitized.stripped_controls,
        }
    }
}

fn reserve_output_bytes(counter: &AtomicU64, maximum: u64, amount: u64) -> bool {
    counter
        .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(amount).filter(|next| *next <= maximum)
        })
        .is_ok()
}

async fn drain_stderr(
    mut stderr: ChildStderr,
    buffer: Arc<Mutex<DiagnosticBuffer>>,
    output_bytes: Arc<AtomicU64>,
    maximum_output_bytes: u64,
    violation: Arc<Mutex<Option<ResourceViolation>>>,
    process_group: Option<u32>,
) {
    const STDERR_RATE_BYTES_PER_SECOND: u64 = 256 * 1024;
    const STDERR_BURST_BYTES: u64 = 1024 * 1024;

    let mut rate = TokenBucket::new(
        STDERR_RATE_BYTES_PER_SECOND,
        STDERR_BURST_BYTES,
        Instant::now(),
    );
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                let read_u64 = u64::try_from(read).unwrap_or(u64::MAX);
                let exceeded = if !rate.consume(Instant::now(), read_u64) {
                    Some(ResourceViolation::OutputRate {
                        bytes_per_second: STDERR_RATE_BYTES_PER_SECOND,
                        burst_bytes: STDERR_BURST_BYTES,
                    })
                } else if !reserve_output_bytes(&output_bytes, maximum_output_bytes, read_u64) {
                    Some(ResourceViolation::Output {
                        limit_bytes: maximum_output_bytes,
                    })
                } else {
                    None
                };
                if let Some(exceeded) = exceeded {
                    if let Ok(mut target) = violation.lock() {
                        target.get_or_insert(exceeded);
                    }
                    if let Some(process_group) = process_group {
                        terminate_process_group(process_group);
                    }
                    return;
                }
                if let Ok(mut target) = buffer.lock() {
                    target.push(&chunk[..read]);
                }
            }
        }
    }
}

fn parse_ps_cpu_time(value: &str) -> Option<u64> {
    let (days, clock): (u64, &str) = if let Some((days, clock)) = value.split_once('-') {
        (days.parse().ok()?, clock)
    } else {
        (0, value)
    };
    let parts = clock
        .split(':')
        .map(|part| part.split('.').next()?.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    let seconds = match parts.as_slice() {
        [minutes, seconds] => minutes.checked_mul(60)?.checked_add(*seconds)?,
        [hours, minutes, seconds] => hours
            .checked_mul(3_600)?
            .checked_add(minutes.checked_mul(60)?)?
            .checked_add(*seconds)?,
        _ => return None,
    };
    days.checked_mul(86_400)?.checked_add(seconds)
}

/// The whole process table, folded to one `(rss_kib, cpu_seconds)` pair per
/// process group.
///
/// A group is a key here whether or not anything is watching it: the table is
/// sampled ONCE per interval for every watcher (see [`ProcessTableSampler`]),
/// and which groups matter is decided by the watchers reading it, not by the
/// scan.
type ProcessTable = HashMap<u32, (u64, u64)>;

/// Sampling failed. Carried as a plain marker rather than `std::io::Error` so a
/// snapshot can be broadcast to every watcher; the watchers only need to know
/// that accounting is gone, which is fail-closed on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SamplingFailed;

/// One sample of the machine, shared by every watcher taken at that instant.
type ProcessSnapshot = Result<Arc<ProcessTable>, SamplingFailed>;

/// Read `/proc` for the process table.
///
/// Returns `NotFound` where there is no procfs — macOS, and any chroot without
/// one — so the caller falls back to `ps`. Everything else (an unreadable
/// entry, a process that exits mid-scan) is skipped rather than failing the
/// scan: those are ordinary races, not a loss of the accounting mechanism.
///
/// RSS comes from `/proc/<pid>/status` (`VmRSS`, already in KiB) rather than
/// field 24 of `stat` (in PAGES). A page count would have to be multiplied by a
/// page size this crate cannot ask for without another dependency, and guessing
/// 4 KiB on a 16 KiB-page kernel under-reports memory by 4x — an error in the
/// direction that lets a worker over its limit keep running.
fn sample_process_table_procfs() -> std::io::Result<ProcessTable> {
    let mut table = ProcessTable::new();
    for entry in std::fs::read_dir("/proc")? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue; // exited mid-scan
        };
        // `comm` is field 2, parenthesised, and may itself contain spaces and
        // ')'. Everything after the LAST ')' is unambiguous, so fields are
        // counted from there: `rest[0]` is field 3 (state).
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let fields = rest.split_whitespace().collect::<Vec<_>>();
        let field = |one_based: usize| fields.get(one_based - 3).copied();
        let Some(group) = field(5).and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let (Some(utime), Some(stime)) = (
            field(14).and_then(|value| value.parse::<u64>().ok()),
            field(15).and_then(|value| value.parse::<u64>().ok()),
        ) else {
            continue;
        };
        // `utime`/`stime` are in USER_HZ, which the kernel fixes at 100 for the
        // /proc ABI on every architecture this ships to. Where it is larger
        // (alpha, ia64), dividing by 100 OVER-states CPU seconds, which spends
        // a worker's budget early — the safe direction for a limit.
        let cpu_seconds = utime.saturating_add(stime) / 100;
        let rss_kib = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|status| {
                status.lines().find_map(|line| {
                    line.strip_prefix("VmRSS:")?
                        .split_whitespace()
                        .next()?
                        .parse::<u64>()
                        .ok()
                })
            })
            .unwrap_or(0);
        let slot = table.entry(group).or_insert((0, 0));
        slot.0 = slot.0.saturating_add(rss_kib);
        slot.1 = slot.1.saturating_add(cpu_seconds);
    }
    Ok(table)
}

/// Read the process table by running `ps` once.
///
/// The fallback for hosts without procfs. `ps` is absent from distroless and
/// busybox-less images, where this errors — and an error here is a LOST
/// ACCOUNTING MECHANISM, which the watchdog treats as a violation. That is
/// deliberate and stays that way; procfs above is what keeps those images
/// working, rather than letting an unmeasurable worker run unmeasured.
fn sample_process_table_ps() -> std::io::Result<ProcessTable> {
    let ps = ["/bin/ps", "/usr/bin/ps"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "ps unavailable"))?;
    let output = std::process::Command::new(ps)
        .args(["-axo", "pgid=,rss=,time="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other("ps process sampling failed"));
    }
    let mut table = ProcessTable::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let Some(group) = fields.next().and_then(|field| field.parse::<u32>().ok()) else {
            continue;
        };
        let Some(rss) = fields.next().and_then(|field| field.parse::<u64>().ok()) else {
            continue;
        };
        let Some(cpu) = fields.next().and_then(parse_ps_cpu_time) else {
            continue;
        };
        let slot = table.entry(group).or_insert((0, 0));
        slot.0 = slot.0.saturating_add(rss);
        slot.1 = slot.1.saturating_add(cpu);
    }
    Ok(table)
}

/// One process-table sample: procfs where it exists, `ps` where it does not.
///
/// The procfs probe is a runtime check, not a `cfg`, so both readers are
/// compiled and reachable on every platform. A `cfg` would leave one of them
/// dead code on the other platform's CI lint.
fn sample_process_table() -> std::io::Result<ProcessTable> {
    PROCESS_TABLE_SCANS.fetch_add(1, Ordering::Relaxed);
    match sample_process_table_procfs() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => sample_process_table_ps(),
        other => other,
    }
}

/// How many whole-machine scans have been taken in this process. The cost this
/// module exists to bound is a scan, so the count of them is the thing a test
/// can hold to a number — see
/// `one_process_table_scan_serves_every_watchdog`.
static PROCESS_TABLE_SCANS: AtomicU64 = AtomicU64::new(0);

/// The single process-table scan that every worker watchdog reads.
///
/// # Why this exists
///
/// Each watchdog used to scan the WHOLE table for itself, every 250ms, to
/// answer a question about one process group: with N workers the host paid N
/// full scans per interval, and on this machine one `ps -axo pgid=,rss=,time=`
/// over 1 122 processes costs ~88ms — so eight workers spent more than three
/// CPU-seconds per wall second on accounting alone.
///
/// # Freshness is NOT traded away
///
/// This is not a cache with a staleness window. The sampler scans on the same
/// 250ms period the watchdogs used, and every watchdog is woken BY the new
/// snapshot instead of by its own timer, so each decision still acts on a
/// sample taken within the current interval — the same worst-case detection
/// latency as before, at one scan instead of N.
///
/// The scan runs only while at least one watchdog is subscribed; the last
/// unsubscribe stops it, so an idle host runs no sampler at all.
struct ProcessTableSampler {
    snapshots: tokio::sync::watch::Sender<Option<ProcessSnapshot>>,
    subscribers: usize,
    task: Option<JoinHandle<()>>,
}

static PROCESS_TABLE_SAMPLER: Mutex<Option<ProcessTableSampler>> = Mutex::new(None);

/// How often the shared sampler scans, and therefore how often each watchdog
/// re-decides. Unchanged from the per-watchdog interval it replaces.
const PROCESS_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

/// How long a watchdog waits for a sample before concluding the accounting
/// mechanism is gone rather than merely slow.
///
/// This is a liveness deadline, not a scheduling one. It was four sample
/// intervals — one second — which conflates two different facts. Where there is
/// no procfs, every sample forks `ps` and reads the whole process table; on a
/// loaded machine that alone can take longer than a second, and the shared
/// sampler serialises it across all watchers, so an ordinarily busy host looked
/// exactly like a dead sampler. The result was a healthy worker killed and
/// reported as `AccountingUnavailable` because the box was busy.
///
/// Ten seconds still bounds the wait and still fails closed on a sampler that
/// has genuinely stopped — the two shapes that mean the mechanism is really
/// gone, a failed scan and a closed channel, are reported directly and are not
/// subject to this deadline at all. The CPU cap is independently enforced as a
/// hard `RLIMIT_CPU`, so the extra latency does not widen the CPU limit; it
/// delays only the memory verdict, and only on a host already too busy to
/// sample.
const PROCESS_SAMPLE_STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// A watchdog's subscription. Dropping it releases the sampler, which stops
/// scanning once no watchdog is left.
struct ProcessSampleSubscription {
    snapshots: tokio::sync::watch::Receiver<Option<ProcessSnapshot>>,
}

impl ProcessSampleSubscription {
    fn new() -> Self {
        let mut guard = PROCESS_TABLE_SAMPLER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sampler = guard.get_or_insert_with(|| ProcessTableSampler {
            snapshots: tokio::sync::watch::channel(None).0,
            subscribers: 0,
            task: None,
        });
        sampler.subscribers += 1;
        let snapshots = sampler.snapshots.subscribe();
        if sampler.task.is_none() {
            let publisher = sampler.snapshots.clone();
            sampler.task = Some(tokio::spawn(async move {
                loop {
                    sleep(PROCESS_SAMPLE_INTERVAL).await;
                    let sample = tokio::task::spawn_blocking(sample_process_table).await;
                    let snapshot = match sample {
                        Ok(Ok(table)) => Ok(Arc::new(table)),
                        // A scan that could not be taken at all, and a blocking
                        // task that died, are the same fact to a watchdog.
                        Ok(Err(_)) | Err(_) => Err(SamplingFailed),
                    };
                    if publisher.send(Some(snapshot)).is_err() {
                        return;
                    }
                }
            }));
        }
        Self { snapshots }
    }

    /// The next sample taken after this call. `None` once the sampler is gone.
    async fn next(&mut self) -> Option<ProcessSnapshot> {
        self.snapshots.changed().await.ok()?;
        self.snapshots.borrow_and_update().clone()
    }
}

impl Drop for ProcessSampleSubscription {
    fn drop(&mut self) {
        let mut guard = PROCESS_TABLE_SAMPLER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(sampler) = guard.as_mut() else {
            return;
        };
        sampler.subscribers = sampler.subscribers.saturating_sub(1);
        if sampler.subscribers == 0 {
            if let Some(task) = sampler.task.take() {
                task.abort();
            }
            *guard = None;
        }
    }
}

async fn watch_process_resources(
    process_group: u32,
    memory_limit_mb: u64,
    cpu_limit_seconds: u64,
    violation: Arc<Mutex<Option<ResourceViolation>>>,
) {
    let memory_limit_kib = memory_limit_mb.saturating_mul(1_024);
    let mut samples = ProcessSampleSubscription::new();
    loop {
        // A sample that never arrives is a lost accounting mechanism too — the
        // one shape a shared sampler could add that a private timer could not,
        // so it is bounded rather than waited on forever. Bounded generously:
        // see `PROCESS_SAMPLE_STALL_TIMEOUT` for why a late sample must not be
        // read as a dead one.
        let sample = timeout(PROCESS_SAMPLE_STALL_TIMEOUT, samples.next()).await;
        let (rss_kib, cpu_seconds) = match sample {
            Ok(Some(Ok(table))) => match table.get(&process_group) {
                Some(sample) => *sample,
                // The group is gone: nothing left to account for or to kill.
                None => return,
            },
            Ok(Some(Err(SamplingFailed))) | Ok(None) | Err(_) => {
                // Losing the accounting mechanism is itself fail-closed.
                if let Ok(mut target) = violation.lock() {
                    *target = Some(ResourceViolation::AccountingUnavailable);
                }
                terminate_process_group(process_group);
                return;
            }
        };
        let exceeded = if memory_limit_kib > 0 && rss_kib > memory_limit_kib {
            Some(ResourceViolation::Memory {
                limit_mb: memory_limit_mb,
            })
        } else if cpu_limit_seconds > 0 && cpu_seconds >= cpu_limit_seconds {
            Some(ResourceViolation::Cpu {
                limit_seconds: cpu_limit_seconds,
            })
        } else {
            None
        };
        if let Some(exceeded) = exceeded {
            if let Ok(mut target) = violation.lock() {
                *target = Some(exceeded);
            }
            terminate_process_group(process_group);
            return;
        }
    }
}

fn redact_sensitive_values(input: &str) -> String {
    const MARKERS: &[&str] = &[
        "token=",
        "token:",
        "secret=",
        "secret:",
        "password=",
        "password:",
        "authorization=",
        "authorization:",
        "api_key=",
        "api-key=",
    ];
    let mut output = String::with_capacity(input.len());
    for line in input.lines() {
        let lowercase = line.to_ascii_lowercase();
        let marker = MARKERS
            .iter()
            .filter_map(|marker| lowercase.find(marker).map(|index| (index, marker.len())))
            .min_by_key(|(index, _)| *index);
        if let Some((index, marker_len)) = marker {
            output.push_str(&line[..index + marker_len]);
            output.push_str("[REDACTED]");
        } else {
            output.push_str(line);
        }
        output.push('\n');
    }
    if !input.ends_with('\n') {
        output.pop();
    }
    output
}

fn wire_message(kind: &str, message_id: String) -> UiWireMessage {
    UiWireMessage {
        kind: kind.into(),
        message_id,
        snapshot: None,
        patch_batch: None,
        event: None,
        action: None,
        subscription: None,
        unsubscription: None,
        projection: None,
        action_result: None,
        cancellation: None,
        dispose: None,
        viewport: None,
        resync: None,
        hot_reload: None,
        capabilities: None,
        selection: None,
        contributions: Vec::new(),
        theme: None,
        error: None,
        extensions: Default::default(),
    }
}

#[derive(Debug, Clone)]
struct TokenBucket {
    refill_per_second: f64,
    capacity: f64,
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(rate: u64, burst: u64, now: Instant) -> Self {
        let capacity = rate.saturating_add(burst) as f64;
        Self {
            refill_per_second: rate as f64,
            capacity,
            tokens: capacity,
            last_refill: now,
        }
    }

    fn available(&mut self, now: Instant) -> u64 {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_second).min(self.capacity);
        self.last_refill = now;
        self.tokens.floor().clamp(0.0, u64::MAX as f64) as u64
    }

    fn consume(&mut self, now: Instant, amount: u64) -> bool {
        let _ = self.available(now);
        if amount as f64 > self.tokens {
            return false;
        }
        self.tokens -= amount as f64;
        true
    }
}

/// A single ready component process. All reads and writes are serialized through
/// `&mut self`, preserving frame order and making shutdown deterministic.
pub struct UiWorker {
    plugin_id: String,
    origin: String,
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr_buffer: Arc<Mutex<DiagnosticBuffer>>,
    stderr_task: Option<JoinHandle<()>>,
    resource_task: Option<JoinHandle<()>>,
    resource_violation: Arc<Mutex<Option<ResourceViolation>>>,
    output_bytes: Arc<AtomicU64>,
    maximum_output_bytes: u64,
    config: UiWorkerConfig,
    limits: UiHardLimits,
    selection: Option<UiCapabilitySelection>,
    declared_capabilities: HashSet<String>,
    declared_contributions: HashMap<String, String>,
    started: Instant,
    message_sequence: u64,
    received_messages: u64,
    message_bucket: TokenBucket,
    byte_bucket: TokenBucket,
    health: WorkerHealth,
    failure_recorded: bool,
    disposed: bool,
    _admission: Option<UiWorkerAdmission>,
}

impl UiWorker {
    fn from_child(
        mut child: Child,
        spec: SandboxProcessSpec,
        launch: UiWorkerLaunch,
        mut config: UiWorkerConfig,
        health: WorkerHealth,
        admission: UiWorkerAdmission,
    ) -> Result<Self, UiWorkerError> {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| UiWorkerError::InvalidRuntime("worker stdin is unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| UiWorkerError::InvalidRuntime("worker stdout is unavailable".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| UiWorkerError::InvalidRuntime("worker stderr is unavailable".into()))?;
        config.maximum_lifetime = config.maximum_lifetime.min(spec.wall_clock());
        config.ready_timeout = config.ready_timeout.min(config.maximum_lifetime);
        config.stderr_bytes = config.stderr_bytes.min(spec.output_cap_bytes());
        let stderr_buffer = Arc::new(Mutex::new(DiagnosticBuffer::new(config.stderr_bytes)));
        let resource_violation = Arc::new(Mutex::new(None));
        let process_group = child.id();
        let maximum_output_bytes = u64::try_from(spec.output_cap_bytes()).unwrap_or(u64::MAX);
        let output_bytes = Arc::new(AtomicU64::new(0));
        let stderr_task = tokio::spawn(drain_stderr(
            stderr,
            Arc::clone(&stderr_buffer),
            Arc::clone(&output_bytes),
            maximum_output_bytes,
            Arc::clone(&resource_violation),
            process_group,
        ));
        let resource_task = process_group.map(|process_group| {
            tokio::spawn(watch_process_resources(
                process_group,
                launch.memory_limit_mb,
                launch.cpu_limit_seconds,
                Arc::clone(&resource_violation),
            ))
        });
        let now = Instant::now();
        let message_bucket = TokenBucket::new(
            u64::from(config.message_rate_per_second),
            u64::from(config.message_rate_burst),
            now,
        );
        let byte_bucket =
            TokenBucket::new(config.byte_rate_per_second, config.byte_rate_burst, now);
        Ok(Self {
            plugin_id: launch.plugin_id,
            origin: spec.origin().to_string(),
            child,
            stdin,
            stdout,
            stderr_buffer,
            stderr_task: Some(stderr_task),
            resource_task,
            resource_violation,
            output_bytes,
            maximum_output_bytes,
            limits: config.hard_limits,
            config,
            selection: None,
            declared_capabilities: launch.declared_capabilities,
            declared_contributions: launch.declared_contributions,
            started: now,
            message_sequence: 0,
            received_messages: 0,
            message_bucket,
            byte_bucket,
            health,
            failure_recorded: false,
            disposed: false,
            _admission: Some(admission),
        })
    }

    #[cfg(test)]
    fn from_test_child(
        mut child: Child,
        plugin_id: &str,
        config: UiWorkerConfig,
        health: WorkerHealth,
    ) -> Self {
        let stdin = child.stdin.take().expect("fixture stdin");
        let stdout = child.stdout.take().expect("fixture stdout");
        let stderr = child.stderr.take().expect("fixture stderr");
        let stderr_buffer = Arc::new(Mutex::new(DiagnosticBuffer::new(config.stderr_bytes)));
        let resource_violation = Arc::new(Mutex::new(None));
        let output_bytes = Arc::new(AtomicU64::new(0));
        let maximum_output_bytes = u64::try_from(config.stderr_bytes).unwrap_or(u64::MAX);
        let stderr_task = tokio::spawn(drain_stderr(
            stderr,
            Arc::clone(&stderr_buffer),
            Arc::clone(&output_bytes),
            maximum_output_bytes,
            Arc::clone(&resource_violation),
            child.id(),
        ));
        let now = Instant::now();
        let message_bucket = TokenBucket::new(
            u64::from(config.message_rate_per_second),
            u64::from(config.message_rate_burst),
            now,
        );
        let byte_bucket =
            TokenBucket::new(config.byte_rate_per_second, config.byte_rate_burst, now);
        Self {
            plugin_id: plugin_id.into(),
            origin: format!("ui-component:{plugin_id}"),
            child,
            stdin,
            stdout,
            stderr_buffer,
            stderr_task: Some(stderr_task),
            resource_task: None,
            resource_violation,
            output_bytes,
            maximum_output_bytes,
            limits: config.hard_limits,
            config,
            selection: None,
            declared_capabilities: HashSet::new(),
            declared_contributions: HashMap::new(),
            started: now,
            message_sequence: 0,
            received_messages: 0,
            message_bucket,
            byte_bucket,
            health,
            failure_recorded: false,
            disposed: false,
            _admission: None,
        }
    }

    async fn handshake(&mut self, host_capabilities: UiCapabilities) -> Result<(), UiWorkerError> {
        let mut offer = wire_message("capabilities", self.next_message_id("host-capabilities"));
        offer.capabilities = Some(host_capabilities.clone());
        self.write(offer).await?;

        let worker_offer = self.read_for_readiness().await?;
        if worker_offer.kind != "capabilities" {
            return Err(UiWorkerError::Handshake(format!(
                "expected capabilities, received {:?}",
                worker_offer.kind
            )));
        }
        let worker_capabilities = worker_offer.capabilities.ok_or_else(|| {
            UiWorkerError::Handshake("worker capability message has no capabilities".into())
        })?;
        for capability in &worker_capabilities.capabilities {
            if !self.declared_capabilities.contains(capability.as_str()) {
                return Err(UiWorkerError::Handshake(format!(
                    "worker requested undeclared capability {:?}",
                    capability.as_str()
                )));
            }
        }
        let selection = host_capabilities
            .negotiate(&worker_capabilities)
            .map_err(|error| UiWorkerError::Handshake(error.to_string()))?;
        self.limits = self.config.hard_limits.intersection(selection.limits);
        let mut selected = wire_message(
            "capabilitySelection",
            self.next_message_id("capability-selection"),
        );
        selected.selection = Some(selection.clone());
        self.write(selected).await?;

        let ready = self.read_for_readiness().await?;
        if ready.kind != "worker.ready" {
            return Err(UiWorkerError::Handshake(format!(
                "expected worker.ready, received {:?}",
                ready.kind
            )));
        }
        self.selection = Some(selection);
        Ok(())
    }

    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    #[must_use]
    pub fn selection(&self) -> Option<&UiCapabilitySelection> {
        self.selection.as_ref()
    }

    #[must_use]
    pub fn limits(&self) -> UiHardLimits {
        self.limits
    }

    /// Receive the next producer message. Idle periods automatically perform a
    /// ping/pong liveness probe; any valid worker message demonstrates liveness.
    pub async fn next_signal(&mut self) -> Result<UiWorkerSignal, UiWorkerError> {
        let mut result = self.next_signal_inner().await;
        if let Ok(mut violation) = self.resource_violation.lock() {
            if let Some(violation) = violation.take() {
                result = Err(violation.into_error());
            }
        }
        if result.is_err() {
            self.record_failure();
            self.terminate().await;
        }
        result
    }

    async fn next_signal_inner(&mut self) -> Result<UiWorkerSignal, UiWorkerError> {
        let remaining = self.remaining_lifetime()?;
        let idle_wait = self.config.heartbeat_interval.min(remaining);
        let message = match timeout(idle_wait, self.read_raw()).await {
            Ok(result) => result?,
            Err(_) if idle_wait == remaining => {
                return Err(UiWorkerError::LifetimeExceeded(
                    self.config.maximum_lifetime,
                ));
            }
            Err(_) => {
                self.send_control("host.ping", json!({})).await?;
                let remaining = self.remaining_lifetime()?;
                let heartbeat_wait = self.config.heartbeat_timeout.min(remaining);
                match timeout(heartbeat_wait, self.read_raw()).await {
                    Ok(result) => result?,
                    Err(_) if heartbeat_wait == remaining => {
                        return Err(UiWorkerError::LifetimeExceeded(
                            self.config.maximum_lifetime,
                        ));
                    }
                    Err(_) => {
                        return Err(UiWorkerError::HeartbeatTimeout(
                            self.config.heartbeat_timeout,
                        ));
                    }
                }
            }
        };
        self.validate_inbound_direction(&message)?;
        match message.kind.as_str() {
            "worker.pong" | "worker.ping" => {
                if message.kind == "worker.ping" {
                    self.send_control("host.pong", json!({})).await?;
                }
                Ok(UiWorkerSignal::Heartbeat)
            }
            "resync" => {
                let request = message.resync.expect("validated resync has payload");
                Ok(UiWorkerSignal::ResyncRequested {
                    document_id: Some(request.document_id),
                    revision: request.known_revision,
                    reason: None,
                })
            }
            "worker.resync" => {
                let control = message.extensions.get("control");
                let document_id = control
                    .and_then(|value| value.get("documentId"))
                    .and_then(Value::as_str)
                    .map(UiDocumentId::from);
                let revision = control
                    .and_then(|value| value.get("revision"))
                    .and_then(Value::as_u64)
                    .map(UiRevision);
                let reason = control
                    .and_then(|value| value.get("reason"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Ok(UiWorkerSignal::ResyncRequested {
                    document_id,
                    revision,
                    reason,
                })
            }
            "worker.reloaded" => Ok(UiWorkerSignal::Reloaded),
            _ => Ok(UiWorkerSignal::Message(Box::new(message))),
        }
    }

    /// Immediately cancel the worker and its process group. Intentional host
    /// cancellation is not counted as a crash for restart/circuit policy.
    pub async fn cancel(&mut self) {
        if self.disposed {
            return;
        }
        self.disposed = true;
        self.terminate().await;
        self.health.success();
    }

    /// Kill a worker after a worker-attributable protocol/broker failure.
    /// Unlike intentional host cancellation, this records exactly one failure
    /// and deliberately does not clear supervisor backoff/circuit history.
    pub async fn fail_and_cancel(&mut self) {
        if self.disposed {
            return;
        }
        self.record_failure();
        self.disposed = true;
        self.terminate().await;
    }

    /// Forward a host-normalized semantic event to the component.
    pub async fn send_event(&mut self, event: UiEvent) -> Result<(), UiWorkerError> {
        self.ensure_protocol(event.protocol_version)?;
        let mut message = wire_message("event", self.next_message_id("event"));
        message.event = Some(event);
        self.write(message).await
    }

    /// Deliver latest-wins state for an authorized mediated subscription.
    pub async fn send_projection(
        &mut self,
        projection: UiProjectionUpdate,
    ) -> Result<(), UiWorkerError> {
        let mut message = wire_message("projection", self.next_message_id("projection"));
        message.projection = Some(projection);
        self.write(message).await
    }

    /// Return the result of a daemon-authorized component action. An invocation
    /// emitted by the worker is never authority by itself.
    pub async fn send_action_result(
        &mut self,
        result: UiActionResult,
    ) -> Result<(), UiWorkerError> {
        let mut message = wire_message("actionResult", self.next_message_id("action-result"));
        message.action_result = Some(result);
        self.write(message).await
    }

    /// Cancel one known pending invocation without terminating the worker.
    pub async fn send_action_cancellation(
        &mut self,
        cancellation: codypendent_protocol::UiActionCancellation,
    ) -> Result<(), UiWorkerError> {
        let mut message = wire_message("cancelAction", self.next_message_id("cancel-action"));
        message.cancellation = Some(cancellation);
        self.write(message).await
    }

    /// Ask the worker for an authoritative snapshot after a stale/rejected
    /// patch, reconnect, or renderer recovery.
    pub async fn request_resync(
        &mut self,
        document_id: &UiDocumentId,
        known_revision: Option<UiRevision>,
        _reason: impl Into<String>,
    ) -> Result<(), UiWorkerError> {
        let mut message = wire_message("resync", self.next_message_id("resync"));
        message.resync = Some(UiResyncRequest {
            document_id: document_id.clone(),
            known_revision,
        });
        self.write(message).await
    }

    /// Tell the worker its verified bundle changed. The worker must discard
    /// component state and answer with `worker.reloaded`, followed by snapshots.
    pub async fn hot_reload(
        &mut self,
        generation: u64,
        changed_modules: Vec<String>,
    ) -> Result<(), UiWorkerError> {
        let mut message = wire_message("hotReload", self.next_message_id("hot-reload"));
        message.hot_reload = Some(UiHotReload {
            generation,
            changed_modules,
        });
        self.write(message).await
    }

    /// Notify a component that a renderer viewport changed.
    pub async fn update_viewport(&mut self, viewport: UiViewport) -> Result<(), UiWorkerError> {
        let mut message = wire_message("viewport", self.next_message_id("viewport"));
        message.viewport = Some(viewport);
        self.write(message).await
    }

    /// Unmount one document without terminating the worker that may own other
    /// contributed surfaces.
    pub async fn dispose_document(
        &mut self,
        document_id: UiDocumentId,
        revision: UiRevision,
    ) -> Result<(), UiWorkerError> {
        let mut message = wire_message("dispose", self.next_message_id("dispose"));
        message.dispose = Some(UiDispose {
            document_id,
            revision,
        });
        self.write(message).await
    }

    /// Graceful dispose with a bounded acknowledgement/exit wait, followed by a
    /// process-group kill and reap. This is idempotent.
    pub async fn shutdown(&mut self) -> Result<(), UiWorkerError> {
        if self.disposed {
            return Ok(());
        }
        self.disposed = true;
        let _ = self.send_control("host.dispose", json!({})).await;
        let deadline = Instant::now() + self.config.shutdown_timeout;
        loop {
            if let Some(_status) = self.child.try_wait().map_err(UiWorkerError::Spawn)? {
                self.finish_stderr().await;
                self.health.success();
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match timeout(remaining.min(Duration::from_millis(100)), self.read_raw()).await {
                Ok(Ok(message)) if message.kind == "worker.disposed" => {
                    let exit_remaining = deadline.saturating_duration_since(Instant::now());
                    let _ = timeout(exit_remaining, self.child.wait()).await;
                    self.terminate().await;
                    self.health.success();
                    return Ok(());
                }
                Ok(Err(UiWorkerError::UnexpectedEof)) => {
                    let _ = self.child.wait().await;
                    self.finish_stderr().await;
                    self.health.success();
                    return Ok(());
                }
                Ok(Err(error)) => {
                    self.record_failure();
                    self.terminate().await;
                    return Err(error);
                }
                _ => {}
            }
        }
        self.terminate().await;
        self.health.success();
        Ok(())
    }

    #[must_use]
    pub fn diagnostics(&self) -> UiWorkerDiagnostics {
        self.stderr_buffer
            .lock()
            .map(|buffer| buffer.snapshot(&self.origin))
            .unwrap_or_default()
    }

    async fn read_raw(&mut self) -> Result<UiWireMessage, UiWorkerError> {
        self.enforce_lifetime()?;
        let now = Instant::now();
        if !self.message_bucket.consume(now, 1) {
            self.record_failure();
            return Err(UiWorkerError::MessageRateExceeded);
        }
        let available_bytes = self.byte_bucket.available(now);
        let byte_bucket = &mut self.byte_bucket;
        let output_bytes = Arc::clone(&self.output_bytes);
        let maximum_output_bytes = self.maximum_output_bytes;
        let resource_violation = Arc::clone(&self.resource_violation);
        let message =
            read_ui_message_with_limits_and_gate(&mut self.stdout, &self.limits, |length| {
                let length_u64 = u64::try_from(length).unwrap_or(u64::MAX);
                if !byte_bucket.consume(now, length_u64) {
                    return Err(UiFramingError::ByteBudgetExceeded {
                        length,
                        available: usize::try_from(available_bytes).unwrap_or(usize::MAX),
                    });
                }
                if !reserve_output_bytes(&output_bytes, maximum_output_bytes, length_u64) {
                    if let Ok(mut target) = resource_violation.lock() {
                        target.get_or_insert(ResourceViolation::Output {
                            limit_bytes: maximum_output_bytes,
                        });
                    }
                    return Err(UiFramingError::ByteBudgetExceeded {
                        length,
                        available: 0,
                    });
                }
                Ok(())
            })
            .await?
            .ok_or_else(|| {
                self.record_failure();
                UiWorkerError::UnexpectedEof
            })?;
        self.received_messages = self.received_messages.saturating_add(1);
        if self.received_messages > self.config.maximum_messages {
            self.record_failure();
            return Err(UiWorkerError::MessageBudgetExceeded);
        }
        Ok(message)
    }

    async fn read_for_readiness(&mut self) -> Result<UiWireMessage, UiWorkerError> {
        let remaining = self.remaining_lifetime()?;
        let readiness_wait = self.config.ready_timeout.min(remaining);
        match timeout(readiness_wait, self.read_raw()).await {
            Ok(result) => result,
            Err(_) if readiness_wait == remaining => Err(UiWorkerError::LifetimeExceeded(
                self.config.maximum_lifetime,
            )),
            Err(_) => Err(UiWorkerError::ReadinessTimeout(self.config.ready_timeout)),
        }
    }

    async fn write(&mut self, message: UiWireMessage) -> Result<(), UiWorkerError> {
        self.enforce_lifetime()?;
        message
            .validate(&self.limits)
            .map_err(UiFramingError::Validation)?;
        if let Err(error) = write_ui_message(&mut self.stdin, &message).await {
            self.record_failure();
            return Err(error.into());
        }
        Ok(())
    }

    async fn send_control(&mut self, kind: &str, control: Value) -> Result<(), UiWorkerError> {
        let mut message = wire_message(kind, self.next_message_id(kind));
        message.extensions.insert("control".into(), control);
        self.write(message).await
    }

    fn validate_inbound_direction(&self, message: &UiWireMessage) -> Result<(), UiWorkerError> {
        match message.kind.as_str() {
            "snapshot" | "patchBatch" | "contributions" | "error" | "resync" | "dispose"
            | "subscription" | "unsubscribe" | "action" | "cancelAction" | "worker.ping"
            | "worker.pong" | "worker.resync" | "worker.reloaded" => {}
            other => {
                return Err(UiWorkerError::DisallowedMessage(format!(
                    "worker-to-host kind {other:?} is not allowed after readiness"
                )));
            }
        }
        if let Some(snapshot) = &message.snapshot {
            self.ensure_protocol(snapshot.document.protocol_version)?;
        }
        if let Some(batch) = &message.patch_batch {
            self.ensure_protocol(batch.protocol_version)?;
        }
        if let Some(subscription) = &message.subscription {
            let required = projection_capability(&subscription.kind).ok_or_else(|| {
                UiWorkerError::DisallowedMessage(format!(
                    "unknown mediated projection kind {:?}",
                    subscription.kind
                ))
            })?;
            if !self.declared_capabilities.contains(required) {
                return Err(UiWorkerError::DisallowedMessage(format!(
                    "projection {:?} requires undeclared capability {required:?}",
                    subscription.kind
                )));
            }
        }
        if (message.action.is_some() || message.cancellation.is_some())
            && !self.declared_capabilities.contains("command-invoke")
        {
            return Err(UiWorkerError::DisallowedMessage(
                "component action requires undeclared capability \"command-invoke\"".into(),
            ));
        }
        if !self.declared_capabilities.contains("command-invoke")
            && message_declares_host_action(message)
        {
            return Err(UiWorkerError::DisallowedMessage(
                "component document declares a host action without approved command-invoke authority"
                    .into(),
            ));
        }
        if message.kind == "contributions" {
            let owner = message
                .extensions
                .get("contributionOwner")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    UiWorkerError::DisallowedMessage(
                        "contribution replacement is missing its authenticated owner".into(),
                    )
                })?;
            if owner != self.plugin_id {
                return Err(UiWorkerError::DisallowedMessage(format!(
                    "contribution replacement spoofed owner {owner:?}"
                )));
            }
            for contribution in &message.contributions {
                if contribution.extension_id.as_str() != self.plugin_id {
                    return Err(UiWorkerError::DisallowedMessage(format!(
                        "contribution {:?} spoofed extension {:?}",
                        contribution.id.as_str(),
                        contribution.extension_id.as_str()
                    )));
                }
                let expected_point = self
                    .declared_contributions
                    .get(contribution.id.as_str())
                    .ok_or_else(|| {
                        UiWorkerError::DisallowedMessage(format!(
                            "undeclared contribution {:?}",
                            contribution.id.as_str()
                        ))
                    })?;
                if expected_point != contribution.point.as_str() {
                    return Err(UiWorkerError::DisallowedMessage(format!(
                        "contribution {:?} changed point from {:?} to {:?}",
                        contribution.id.as_str(),
                        expected_point,
                        contribution.point.as_str()
                    )));
                }
            }
        }
        Ok(())
    }

    fn ensure_protocol(&self, protocol: UiProtocolVersion) -> Result<(), UiWorkerError> {
        if self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.protocol_version != protocol)
        {
            return Err(UiWorkerError::DisallowedMessage(format!(
                "message protocol {}.{} differs from negotiated version",
                protocol.major, protocol.minor
            )));
        }
        Ok(())
    }

    fn enforce_lifetime(&mut self) -> Result<(), UiWorkerError> {
        if self.remaining_lifetime().is_err() {
            self.record_failure();
            return Err(UiWorkerError::LifetimeExceeded(
                self.config.maximum_lifetime,
            ));
        }
        Ok(())
    }

    fn remaining_lifetime(&self) -> Result<Duration, UiWorkerError> {
        self.config
            .maximum_lifetime
            .checked_sub(self.started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(UiWorkerError::LifetimeExceeded(
                self.config.maximum_lifetime,
            ))
    }

    fn next_message_id(&mut self, prefix: &str) -> String {
        self.message_sequence = self.message_sequence.saturating_add(1);
        format!("host-{prefix}-{}", self.message_sequence)
    }

    fn record_failure(&mut self) {
        if !self.failure_recorded {
            self.failure_recorded = true;
            self.health.failure();
        }
    }

    async fn finish_stderr(&mut self) {
        if let Some(task) = self.stderr_task.take() {
            let _ = timeout(Duration::from_secs(1), task).await;
        }
    }

    async fn terminate(&mut self) {
        if let Some(task) = self.resource_task.take() {
            task.abort();
        }
        if let Some(pid) = self.child.id() {
            terminate_process_group(pid);
        }
        let _ = self.child.start_kill();
        let _ = timeout(Duration::from_secs(2), self.child.wait()).await;
        self.finish_stderr().await;
    }
}

const FLAT_ACTION_PROPS: &[&str] = &[
    "action",
    "changeAction",
    "submitAction",
    "selectAction",
    "dismissAction",
    "openAction",
    "closeAction",
    "toggleAction",
    "navigateAction",
    "validateAction",
];

fn node_declares_host_action(node: &codypendent_protocol::UiNode) -> bool {
    !node.props.event_bindings.is_empty()
        || FLAT_ACTION_PROPS.iter().any(|key| {
            node.props
                .extension
                .get(*key)
                .is_some_and(|value| !value.is_null())
        })
        || node.children.iter().any(node_declares_host_action)
        || node
            .fallback
            .as_deref()
            .is_some_and(node_declares_host_action)
}

fn message_declares_host_action(message: &UiWireMessage) -> bool {
    message
        .snapshot
        .as_ref()
        .is_some_and(|snapshot| node_declares_host_action(&snapshot.document.root))
        || message.patch_batch.as_ref().is_some_and(|batch| {
            batch.patches.iter().any(|patch| {
                patch.node.as_ref().is_some_and(node_declares_host_action)
                    || patch.props.as_ref().is_some_and(|props| {
                        FLAT_ACTION_PROPS
                            .iter()
                            .any(|key| props.set.get(*key).is_some_and(|value| !value.is_null()))
                            || props.set.get("eventBindings").is_some_and(|value| {
                                value.as_array().is_some_and(|items| !items.is_empty())
                            })
                    })
            })
        })
}

fn projection_capability(kind: &str) -> Option<&'static str> {
    match kind {
        "artifact" => Some("artifact-read"),
        "context" | "session" => Some("context-read"),
        "run" => Some("run-read"),
        // A run's blackboard is part of that workflow run's observable state:
        // same resource id, same ownership join, same read-only authority.
        "workflow" | "blackboard" => Some("workflow-read"),
        "command" => Some("command-invoke"),
        _ => None,
    }
}

impl Drop for UiWorker {
    fn drop(&mut self) {
        if let Some(task) = self.resource_task.take() {
            task.abort();
        }
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
        if let Some(pid) = self.child.id() {
            terminate_process_group(pid);
        }
        let _ = self.child.start_kill();
        if !self.disposed {
            self.record_failure();
        }
    }
}

/// SIGKILL the worker's whole process group.
///
/// A `kill(-pgid, SIGKILL)` syscall (via `codypendent_sandbox`), not a
/// `/bin/kill` subprocess: that binary is absent on minimal images, where the
/// sweep silently did nothing and every grandchild of the worker leaked. It
/// also removes a synchronous fork/exec/wait from an async fn and from `Drop`.
///
/// Both callers read the pid from `Child::id`, which tokio makes `None` once the
/// child has been reaped — so this is never handed a pid the host no longer
/// owns, which is what keeps a recycled pgid out of range.
#[cfg(unix)]
fn terminate_process_group(pid: u32) {
    codypendent_sandbox::executor::kill_process_group(pid);
}

#[cfg(not(unix))]
fn terminate_process_group(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_sandbox::{
        checksum_of, parse_manifest, CapabilitySet, RefusingSandbox, UnsignedPolicy,
    };

    /// A busy machine must not look like a dead accounting mechanism.
    ///
    /// The watchdog's wait for the next sample was four sample intervals — one
    /// second. Where there is no procfs each sample forks `ps` and reads the
    /// whole process table, and the shared sampler serialises that across every
    /// watcher, so a merely loaded host tripped the deadline and the worker was
    /// killed and reported as `AccountingUnavailable`. The deadline exists to
    /// catch a sampler that has stopped, and must be far enough above the cost
    /// of sampling that only that can trip it.
    #[test]
    fn the_watchdogs_stall_deadline_leaves_room_for_slow_sampling() {
        assert!(
            PROCESS_SAMPLE_STALL_TIMEOUT >= PROCESS_SAMPLE_INTERVAL * 20,
            "a stall deadline within a few sample intervals kills healthy \
             workers on a busy host: {PROCESS_SAMPLE_STALL_TIMEOUT:?} vs a \
             {PROCESS_SAMPLE_INTERVAL:?} interval"
        );
        // Still bounded: a sampler that has genuinely stopped must still be
        // caught, and well inside the worker's own wall clock.
        assert!(
            PROCESS_SAMPLE_STALL_TIMEOUT < Duration::from_secs(UI_WORKER_WALL_SECONDS),
            "the deadline must still fire long before the worker's wall clock"
        );
    }

    /// A `ui-component` manifest with no `[resources]` block must not inherit
    /// the short plugin-script wall clock.
    ///
    /// It did, and the consequence was not a tighter policy but a broken
    /// component: killed at 60 seconds, circuit opened, backed off, restarted,
    /// forever. The native-process arm has always set a long clock here and
    /// explained why; this arm silently took a default nobody chose.
    #[test]
    fn an_undeclared_wall_clock_does_not_kill_a_ui_worker_every_minute() {
        let script_default = ResourcesSpec::default().wall_seconds;

        // The trap, stated: this is what a manifest without `[resources]` gets.
        assert_eq!(script_default, 60, "the script default moved; revisit this");
        assert!(
            UI_WORKER_WALL_SECONDS > script_default,
            "a persistent UI worker cannot live under a task-length clock"
        );

        assert_eq!(
            ui_worker_wall_seconds(script_default),
            UI_WORKER_WALL_SECONDS,
            "an undeclared wall clock must become the UI worker's, not a script's"
        );
    }

    /// Promoting the wall clock alone left the same kill/circuit-open loop in
    /// place under different numbers.
    ///
    /// A `ui-component` inherited a one-shot script's 30 CPU-seconds — a hard
    /// `RLIMIT_CPU` plus a watchdog — and an 8 MiB LIFETIME output cap. A
    /// worker that lives as long as the surface showing it reaches both by
    /// simply being used, and is then killed mid-use exactly as the 60-second
    /// clock killed it. All three defaults were written for a script; they have
    /// to move together.
    #[test]
    fn an_undeclared_cpu_and_output_cap_do_not_kill_a_ui_worker_either() {
        let defaults = ResourcesSpec::default();
        assert_eq!(
            defaults.cpu_seconds, 30,
            "the script default moved; revisit this"
        );
        assert_eq!(
            defaults.maximum_output_mb, 8,
            "the script default moved; revisit this"
        );

        assert_eq!(
            ui_worker_cpu_seconds(defaults.cpu_seconds),
            UI_WORKER_CPU_SECONDS,
            "an undeclared CPU slice must become the UI worker's, not a script's"
        );
        assert_eq!(
            ui_worker_output_mb(defaults.maximum_output_mb),
            UI_WORKER_OUTPUT_MB,
            "an undeclared output cap must become the UI worker's, not a script's"
        );

        // A declared value is the author describing THIS worker, and stands.
        assert_eq!(ui_worker_cpu_seconds(45), 45);
        assert_eq!(ui_worker_output_mb(16), 16);

        // And the promoted values must actually be more than a script's, or the
        // promotion is decoration.
        assert!(UI_WORKER_CPU_SECONDS > defaults.cpu_seconds);
        assert!(UI_WORKER_OUTPUT_MB > defaults.maximum_output_mb);
    }

    /// A `ui-component` author who does declare a wall clock keeps it: their
    /// manifest's resources describe this very worker.
    #[test]
    fn a_declared_wall_clock_is_still_honored() {
        assert_eq!(ui_worker_wall_seconds(120), 120);
        assert_eq!(ui_worker_wall_seconds(7), 7);
    }

    /// Both arms agree on how long a UI worker may live. They disagreed only
    /// because one of them wrote the number down and the other did not.
    #[test]
    fn a_manifest_without_resources_really_does_default_to_the_short_clock() {
        let manifest = parse_manifest(
            r#"
schema_version = 1
id = "example.ui"
name = "Example"
version = "1.0.0"
kind = "ui-component"
publisher = "test"
scopes = ["repository"]

[security]
checksum = "sha256:0000000000000000000000000000000000000000000000000000000000000000"

[ui]
schema_version = 1
requested_capabilities = []

[ui.compatibility]
protocol = "^1.0"
sdk = "^1.0"

[ui.entrypoints]
shared = "dist/worker.mjs"

[[ui.contributions]]
id = "example.panel"
point = "panel"
renderer = "example.Panel"
targets = ["shared"]
"#,
        )
        .expect("manifest parses");

        assert_eq!(
            manifest.resources.wall_seconds,
            ResourcesSpec::default().wall_seconds,
            "a manifest with no [resources] inherits the default; that is the bug's source"
        );
        assert_eq!(
            ui_worker_wall_seconds(manifest.resources.wall_seconds),
            UI_WORKER_WALL_SECONDS
        );
    }

    fn archive_worker(content: &[u8]) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_ustar();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        archive
            .append_data(&mut header, "dist/worker.mjs", content)
            .unwrap();
        archive
            .into_inner()
            .unwrap()
            .finish()
            .expect("gzip archive")
    }

    fn package_with_worker(content: &[u8]) -> (tempfile::TempDir, Vec<u8>) {
        let package = tempfile::tempdir().unwrap();
        std::fs::create_dir(package.path().join("dist")).unwrap();
        std::fs::write(package.path().join("dist/worker.mjs"), content).unwrap();
        (package, archive_worker(content))
    }

    fn test_runtime() -> UiWorkerRuntime {
        let executable = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let read_root = executable.parent().unwrap().to_path_buf();
        UiWorkerRuntime {
            executable,
            read_root,
        }
    }

    fn fixture_launch(
        installed: &InstalledPlugin,
        artifact: &[u8],
        package_root: &Path,
        target: UiTarget,
        purpose: UiWorkerLaunchPurpose,
    ) -> Result<UiWorkerLaunch, UiWorkerError> {
        UiWorkerLaunch::from_installed_with_runtime(
            installed,
            artifact,
            package_root,
            target,
            test_runtime(),
            purpose,
        )
    }

    fn installed_ui(state: LifecycleState, artifact: &[u8]) -> InstalledPlugin {
        let manifest = parse_manifest(&format!(
            r#"
schema_version = 1
id = "fixture-ui"
name = "Fixture UI"
version = "1.0.0"
kind = "ui-component"
publisher = "test"
scopes = ["repository"]

[security]
checksum = "sha256:{}"

[ui]
schema_version = 1
requested_capabilities = []

[ui.compatibility]
protocol = "^1.0"
sdk = "^1.0"

[ui.entrypoints]
shared = "dist/worker.mjs"

[[ui.contributions]]
id = "fixture.panel"
point = "panel"
renderer = "fixture.Panel"
targets = ["shared"]
"#,
            checksum_of(artifact).trim_start_matches("sha256:")
        ))
        .expect("UI manifest");
        let granted_ui = manifest
            .ui
            .as_ref()
            .map(|ui| ui.requested_capabilities.iter().copied().collect())
            .unwrap_or_default();
        let mut installed = InstalledPlugin::install_disabled(
            manifest,
            artifact,
            None,
            UnsignedPolicy::Allow,
            CapabilitySet::default(),
            granted_ui,
        )
        .expect("fixture installs");
        match state {
            LifecycleState::InstalledDisabled => {}
            LifecycleState::SmokeTested => installed.mark_smoke_tested().unwrap(),
            LifecycleState::Enabled => {
                installed.mark_smoke_tested().unwrap();
                installed.enable("repository").unwrap();
            }
            LifecycleState::Revoked => installed.revoke(),
            LifecycleState::UpdateBlocked => panic!("fixture does not synthesize pending approval"),
        }
        installed
    }

    fn installed_native_ui(artifact: &[u8]) -> InstalledPlugin {
        let manifest = parse_manifest(&format!(
            r#"
schema_version = 1
id = "fixture-native-ui"
name = "Fixture Native UI"
version = "1.0.0"
kind = "native-process"
publisher = "test"
scopes = ["repository"]

[runtime]
command = "bin/native"

[capabilities]
network = ["api.example.invalid:443"]
secrets = ["native-token"]
subprocess = true

[security]
checksum = "sha256:{}"

[ui]
schema_version = 1
requested_capabilities = []

[ui.compatibility]
protocol = "^1.0"
sdk = "^1.0"

[ui.entrypoints]
shared = "dist/worker.mjs"

[[ui.contributions]]
id = "fixture.panel"
point = "panel"
renderer = "fixture.Panel"
targets = ["shared"]
"#,
            checksum_of(artifact).trim_start_matches("sha256:")
        ))
        .expect("native + UI manifest");
        let granted = CapabilitySet::from_spec(&manifest.capabilities);
        let mut installed = InstalledPlugin::install_disabled(
            manifest,
            artifact,
            None,
            UnsignedPolicy::Allow,
            granted,
            BTreeSet::new(),
        )
        .expect("native + UI fixture installs");
        installed.mark_smoke_tested().unwrap();
        installed.enable("repository").unwrap();
        installed
    }

    fn host_capabilities() -> UiCapabilities {
        serde_json::from_value(json!({
            "client": "test",
            "protocolVersions": [{ "major": 1, "minor": 0 }],
            "daemon": {
                "rich_text": true,
                "image_display": false,
                "audio_capture": false,
                "editor_mutations": false,
                "diff_view": true,
                "mouse": false,
                "unicode": true,
                "true_color": true
            },
            "primitives": "*",
            "media": [],
            "colorDepth": "trueColor",
            "keyboard": true,
            "screenReader": false,
            "reducedMotion": false,
            "clipboard": false,
            "viewport": { "width": 80, "height": 24 }
        }))
        .expect("capabilities fixture")
    }

    // Bounds a healthy worker has to clear, as distinct from the ones under
    // test. Three of these tests spawn `node` on the fixture and then hand it a
    // handshake, so `ready_timeout` has to cover a cold ESM start on whatever
    // CPU the runner has left over — which is not a latency this suite is
    // asserting. At two seconds it was asserting exactly that, and CI failed
    // all three with `ReadinessTimeout(2s)` on a commit that touched none of
    // this and passed locally; reproduced here by running them under 64x CPU
    // oversubscription, where the same three fail on the same line. Thirty
    // seconds still catches a worker that never comes up and costs nothing when
    // one does. `maximum_lifetime` has to move with it because readiness waits
    // `ready_timeout.min(remaining lifetime)`, so leaving it at ten would just
    // re-impose the old ceiling one layer down.
    //
    // The millisecond bounds below are a different kind and stay: heartbeat and
    // backoff timing IS what their tests measure.
    fn test_config() -> UiWorkerConfig {
        UiWorkerConfig {
            ready_timeout: Duration::from_secs(30),
            heartbeat_interval: Duration::from_millis(30),
            heartbeat_timeout: Duration::from_millis(30),
            shutdown_timeout: Duration::from_secs(1),
            maximum_lifetime: Duration::from_secs(60),
            initial_restart_backoff: Duration::from_millis(10),
            maximum_restart_backoff: Duration::from_millis(20),
            circuit_cooldown: Duration::from_secs(1),
            ..UiWorkerConfig::default()
        }
    }

    #[test]
    fn aggregate_admission_counts_active_and_smoke_workers_together_and_releases() {
        let supervisor =
            UiWorkerSupervisor::new(Arc::new(RefusingSandbox), test_config()).expect("supervisor");
        let mut active = Vec::new();
        for _ in 0..(MAX_SUPERVISED_UI_WORKERS - 1) {
            active.push(supervisor.try_admit(1).expect("active reservation"));
        }
        let smoke = supervisor.try_admit(1).expect("smoke reservation");
        assert!(matches!(
            supervisor.try_admit(1),
            Err(UiWorkerError::AggregateAdmissionDenied {
                resource: "worker count",
                ..
            })
        ));
        drop(smoke);
        let replacement = supervisor
            .try_admit(1)
            .expect("dropping smoke releases its count and memory");
        drop(replacement);
        drop(active);

        let active = supervisor
            .try_admit(MAX_SUPERVISED_UI_MEMORY_MB - 1)
            .expect("large active reservation");
        let smoke = supervisor.try_admit(1).expect("last memory unit");
        assert!(matches!(
            supervisor.try_admit(1),
            Err(UiWorkerError::AggregateAdmissionDenied {
                resource: "declared memory",
                ..
            })
        ));
        drop((active, smoke));
        assert!(supervisor.try_admit(MAX_SUPERVISED_UI_MEMORY_MB).is_ok());
    }

    fn test_node() -> Option<PathBuf> {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join("node"))
            .find(|candidate| candidate.is_file())
    }

    fn spawn_fixture(mode: &str) -> Option<Child> {
        let node = test_node()?;
        if std::process::Command::new(&node)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()
            .is_none_or(|status| !status.success())
        {
            eprintln!("node unavailable; skipping process fixture");
            return None;
        }
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ui_worker.mjs");
        let mut command = Command::new(node);
        command
            .arg(fixture)
            .arg(mode)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        Some(command.spawn().expect("spawn node fixture"))
    }

    fn worker_from_fixture(
        mode: &str,
        config: UiWorkerConfig,
        circuits: Arc<Mutex<HashMap<String, CircuitState>>>,
    ) -> Option<UiWorker> {
        let child = spawn_fixture(mode)?;
        let health = WorkerHealth {
            circuit_key: "fixture".into(),
            config: config.clone(),
            circuits,
        };
        Some(UiWorker::from_test_child(child, "fixture", config, health))
    }

    #[tokio::test]
    async fn process_worker_handshakes_resyncs_reloads_and_disposes() {
        let circuits = Arc::new(Mutex::new(HashMap::new()));
        let Some(mut worker) = worker_from_fixture("normal", test_config(), circuits) else {
            return;
        };
        worker
            .handshake(host_capabilities())
            .await
            .expect("handshake");
        assert_eq!(
            worker.selection().unwrap().protocol_version,
            UiProtocolVersion::V1
        );

        worker
            .hot_reload(3, vec!["dist/component.js".into()])
            .await
            .expect("reload request");
        assert_eq!(
            worker.next_signal().await.unwrap(),
            UiWorkerSignal::Reloaded
        );

        worker
            .request_resync(&UiDocumentId::from("document"), Some(UiRevision(4)), "test")
            .await
            .expect("resync request");
        assert_eq!(
            worker.next_signal().await.unwrap(),
            UiWorkerSignal::ResyncRequested {
                document_id: Some(UiDocumentId::from("document")),
                revision: Some(UiRevision(4)),
                reason: None,
            }
        );
        worker.shutdown().await.expect("graceful shutdown");
    }

    #[tokio::test]
    async fn worker_attributable_cancellation_preserves_backoff_and_opens_circuit() {
        let circuits = Arc::new(Mutex::new(HashMap::new()));
        let config = test_config();
        for _ in 0..config.circuit_failure_threshold {
            let Some(mut worker) =
                worker_from_fixture("normal", config.clone(), Arc::clone(&circuits))
            else {
                return;
            };
            worker.fail_and_cancel().await;
        }
        {
            let circuits = circuits.lock().unwrap();
            let state = circuits.get("fixture").expect("fixture circuit recorded");
            assert_eq!(
                state.failures.len(),
                config.circuit_failure_threshold as usize
            );
            assert!(state.next_launch.is_some());
            assert!(state.open_until.is_some());
        }
        let supervisor = UiWorkerSupervisor {
            executor: Arc::new(RefusingSandbox),
            resource_launcher: None,
            config: config.clone(),
            circuits,
            admission: Arc::new(Mutex::new(UiWorkerAdmissionState::default())),
        };
        assert!(matches!(
            supervisor.check_circuit("fixture"),
            Err(UiWorkerError::CircuitOpen { .. })
        ));
        tokio::time::sleep(config.circuit_cooldown + Duration::from_millis(20)).await;
        assert!(supervisor.check_circuit("fixture").is_ok());
    }

    #[tokio::test]
    async fn heartbeat_timeout_kills_an_unresponsive_worker_and_opens_circuit() {
        let circuits = Arc::new(Mutex::new(HashMap::new()));
        let mut config = test_config();
        config.circuit_failure_threshold = 1;
        let Some(mut worker) =
            worker_from_fixture("no-pong", config.clone(), Arc::clone(&circuits))
        else {
            return;
        };
        worker
            .handshake(host_capabilities())
            .await
            .expect("handshake");
        assert!(matches!(
            worker.next_signal().await,
            Err(UiWorkerError::HeartbeatTimeout(_))
        ));
        worker.terminate().await;

        let supervisor = UiWorkerSupervisor {
            executor: Arc::new(RefusingSandbox),
            resource_launcher: None,
            config,
            circuits,
            admission: Arc::new(Mutex::new(UiWorkerAdmissionState::default())),
        };
        assert!(matches!(
            supervisor.check_circuit("fixture"),
            Err(UiWorkerError::CircuitOpen { .. })
        ));
        let status = supervisor.circuit_status("fixture").unwrap();
        assert_eq!(status.recent_failures, 1);
        assert!(status.circuit_open_remaining.is_some());
        supervisor.reset_circuit("fixture").unwrap();
        assert_eq!(
            supervisor.circuit_status("fixture").unwrap(),
            UiWorkerCircuitStatus::default()
        );
    }

    #[test]
    fn worker_health_isolated_by_attested_instance_key() {
        let circuits = Arc::new(Mutex::new(HashMap::new()));
        let config = test_config();
        let failing = WorkerHealth {
            circuit_key: "plugin:session-a:terminal:g1".to_owned(),
            config: config.clone(),
            circuits: Arc::clone(&circuits),
        };
        let healthy = WorkerHealth {
            circuit_key: "plugin:session-b:web:g1".to_owned(),
            config,
            circuits: Arc::clone(&circuits),
        };
        failing.failure();
        healthy.success();
        let states = circuits.lock().expect("circuit lock");
        let failed = states
            .get("plugin:session-a:terminal:g1")
            .expect("failed instance keeps its own circuit");
        assert_eq!(failed.failures.len(), 1);
        assert!(failed.next_launch.is_some());
        assert!(!states.contains_key("plugin:session-b:web:g1"));
        assert!(!states.contains_key("plugin"));
    }

    #[tokio::test]
    async fn message_flood_and_wrong_direction_fail_closed() {
        let mut config = test_config();
        config.message_rate_per_second = 1;
        config.message_rate_burst = 2;
        let circuits = Arc::new(Mutex::new(HashMap::new()));
        let Some(mut flood) = worker_from_fixture("flood", config, Arc::clone(&circuits)) else {
            return;
        };
        flood
            .handshake(host_capabilities())
            .await
            .expect("handshake");
        let mut rejected = false;
        for _ in 0..10 {
            if matches!(
                flood.next_signal().await,
                Err(UiWorkerError::MessageRateExceeded)
            ) {
                rejected = true;
                break;
            }
        }
        assert!(rejected, "flood must exceed the token window");
        flood.terminate().await;

        let Some(mut wrong) =
            worker_from_fixture("bad-direction", test_config(), Arc::clone(&circuits))
        else {
            return;
        };
        wrong
            .handshake(host_capabilities())
            .await
            .expect("handshake");
        assert!(matches!(
            wrong.next_signal().await,
            Err(UiWorkerError::DisallowedMessage(_))
        ));
        wrong.terminate().await;

        let Some(mut subscription) =
            worker_from_fixture("bad-subscription", test_config(), Arc::clone(&circuits))
        else {
            return;
        };
        subscription
            .handshake(host_capabilities())
            .await
            .expect("handshake");
        assert!(matches!(
            subscription.next_signal().await,
            Err(UiWorkerError::DisallowedMessage(message))
                if message.contains("artifact-read")
        ));
    }

    #[tokio::test]
    async fn stderr_is_bounded_control_stripped_and_secret_redacted() {
        let circuits = Arc::new(Mutex::new(HashMap::new()));
        let Some(mut worker) = worker_from_fixture("stderr", test_config(), circuits) else {
            return;
        };
        worker
            .handshake(host_capabilities())
            .await
            .expect("handshake");
        tokio::time::sleep(Duration::from_millis(30)).await;
        worker.shutdown().await.expect("shutdown");
        let diagnostics = worker.diagnostics();
        assert!(diagnostics.text.contains("token=[REDACTED]"));
        assert!(!diagnostics.text.contains("do-not-leak"));
        assert!(!diagnostics.text.contains('\u{1b}'));
        assert!(diagnostics.text.contains("ordinary diagnostic"));
    }

    #[tokio::test]
    async fn stderr_flood_terminates_the_worker_at_its_lifetime_output_cap() {
        let circuits = Arc::new(Mutex::new(HashMap::new()));
        let config = UiWorkerConfig {
            stderr_bytes: 32 * 1024,
            ..test_config()
        };
        let Some(mut worker) = worker_from_fixture("stderr-flood", config, circuits) else {
            return;
        };
        worker
            .handshake(host_capabilities())
            .await
            .expect("handshake");
        let error = worker
            .next_signal()
            .await
            .expect_err("stderr flood fails closed");
        assert!(matches!(
            error,
            UiWorkerError::ResourceLimitExceeded {
                resource: "worker output" | "worker stderr rate",
                ..
            }
        ));
        assert!(worker.diagnostics().text.len() <= 32 * 1024);
    }

    #[tokio::test]
    async fn stdout_frames_share_the_lifetime_output_cap() {
        let circuits = Arc::new(Mutex::new(HashMap::new()));
        let config = UiWorkerConfig {
            stderr_bytes: 4 * 1024,
            ..test_config()
        };
        let Some(mut worker) = worker_from_fixture("flood", config, circuits) else {
            return;
        };
        worker
            .handshake(host_capabilities())
            .await
            .expect("handshake");
        let mut failure = None;
        for _ in 0..128 {
            match worker.next_signal().await {
                Ok(_) => continue,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        let error = failure.expect("stdout flood fails closed");
        assert!(matches!(
            error,
            UiWorkerError::ResourceLimitExceeded {
                resource: "worker output",
                ..
            }
        ));
    }

    #[test]
    fn redaction_covers_common_secret_labels_without_touching_normal_lines() {
        let redacted = redact_sensitive_values(
            "Authorization: Bearer abc\npassword=hunter2\nnormal diagnostic",
        );
        assert_eq!(
            redacted,
            "Authorization:[REDACTED]\npassword=[REDACTED]\nnormal diagnostic"
        );
    }

    #[test]
    fn process_cpu_accounting_parses_linux_and_macos_formats() {
        assert_eq!(parse_ps_cpu_time("00:01:02"), Some(62));
        assert_eq!(parse_ps_cpu_time("1:02.75"), Some(62));
        assert_eq!(parse_ps_cpu_time("2-03:04:05"), Some(183_845));
        assert_eq!(parse_ps_cpu_time("not-a-time"), None);
    }

    /// Whichever reader this host has, it must find the test process itself —
    /// otherwise the watchdog's "the group is gone" branch fires for a group
    /// that is very much alive, and every worker is killed one interval after
    /// it starts. That is exactly what a distroless image (no `ps`, procfs
    /// present) got before the procfs reader existed.
    #[test]
    fn the_process_table_reader_available_here_finds_this_process() {
        let table = sample_process_table().expect("a host with either procfs or ps");
        assert!(
            !table.is_empty(),
            "a running host has at least one process group"
        );
        assert!(
            table.values().any(|(rss_kib, _cpu)| *rss_kib > 0),
            "resident memory is read, not left at zero: {table:?}"
        );
    }

    /// One scan serves every watchdog. Eight private timers at the old 250ms
    /// period would take at least eight scans in this window — on this machine
    /// one scan of 1 122 processes costs ~88ms, so that arithmetic was the
    /// whole finding.
    #[tokio::test]
    async fn one_process_table_scan_serves_every_watchdog() {
        let before = PROCESS_TABLE_SCANS.load(Ordering::Relaxed);
        let subscriptions = (0..8)
            .map(|_| ProcessSampleSubscription::new())
            .collect::<Vec<_>>();
        tokio::time::sleep(PROCESS_SAMPLE_INTERVAL * 2 + Duration::from_millis(120)).await;
        let scans = PROCESS_TABLE_SCANS.load(Ordering::Relaxed) - before;
        drop(subscriptions);
        assert!(
            scans >= 1,
            "the shared sampler must actually sample; saw {scans}"
        );
        assert!(
            scans <= 4,
            "eight watchdogs over two intervals must not cost eight scans; saw {scans}"
        );
    }

    #[test]
    fn invalid_runtime_configuration_is_rejected_before_spawn() {
        let config = UiWorkerConfig {
            heartbeat_interval: Duration::ZERO,
            ..UiWorkerConfig::default()
        };
        assert!(matches!(
            UiWorkerSupervisor::new(Arc::new(RefusingSandbox), config),
            Err(UiWorkerError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn token_bucket_refills_only_the_sustained_rate_after_burst() {
        let started = Instant::now();
        let mut bytes = TokenBucket::new(4, 16, started);
        assert!(
            bytes.consume(started, 20),
            "initial rate plus burst is available"
        );
        assert!(
            !bytes.consume(started, 1),
            "drained burst does not reset immediately"
        );
        let one_second = started + Duration::from_secs(1);
        assert_eq!(bytes.available(one_second), 4);
        assert!(!bytes.consume(one_second, 5));
        assert!(bytes.consume(one_second, 4));
        assert_eq!(bytes.available(one_second), 0);
    }

    #[test]
    fn lifetime_output_reservation_is_atomic_and_never_overshoots() {
        let output = AtomicU64::new(0);
        assert!(reserve_output_bytes(&output, 10, 6));
        assert!(!reserve_output_bytes(&output, 10, 5));
        assert_eq!(output.load(Ordering::Acquire), 6);
        assert!(reserve_output_bytes(&output, 10, 4));
        assert_eq!(output.load(Ordering::Acquire), 10);
    }

    #[test]
    fn launch_binds_an_enabled_verified_entrypoint_to_its_package_root() {
        let (package, artifact) = package_with_worker(b"export {};");
        let launch = fixture_launch(
            &installed_ui(LifecycleState::Enabled, &artifact),
            &artifact,
            package.path(),
            UiTarget::Terminal,
            UiWorkerLaunchPurpose::Active,
        )
        .expect("verified shared entrypoint launches for terminal");
        assert_eq!(launch.plugin_id(), "fixture-ui");
        assert!(launch
            .entrypoint()
            .starts_with(std::fs::canonicalize(package.path()).unwrap()));
        assert_eq!(launch.target(), UiTarget::Terminal);
        assert_eq!(launch.profile.network_allowlist, Vec::<String>::new());
        assert_eq!(launch.profile.brokered_secrets, Vec::<String>::new());
        assert!(!launch.profile.allow_subprocess);
    }

    #[test]
    fn native_integration_ui_worker_never_inherits_native_process_authority() {
        let (package, artifact) = package_with_worker(b"export {};");
        let launch = fixture_launch(
            &installed_native_ui(&artifact),
            &artifact,
            package.path(),
            UiTarget::Terminal,
            UiWorkerLaunchPurpose::Active,
        )
        .expect("native integration's separately sandboxed UI launches");
        assert!(launch.profile.network_allowlist.is_empty());
        assert!(launch.profile.brokered_secrets.is_empty());
        assert!(!launch.profile.allow_subprocess);
        assert_eq!(launch.profile.memory_mb, 128);
        assert_eq!(launch.profile.cpu_seconds, 300);
        assert_eq!(launch.profile.wall_seconds, 86_400);
        assert_eq!(launch.profile.maximum_output_mb, 8);
    }

    #[test]
    fn active_launch_rejects_inactive_records() {
        let (package, artifact) = package_with_worker(b"export {};");
        assert!(matches!(
            fixture_launch(
                &installed_ui(LifecycleState::InstalledDisabled, &artifact),
                &artifact,
                package.path(),
                UiTarget::Shared,
                UiWorkerLaunchPurpose::Active,
            ),
            Err(UiWorkerError::InactivePlugin { .. })
        ));
    }

    #[test]
    fn worker_message_budget_is_the_shared_protocol_ceiling() {
        // The SDK worker defaults to exactly these numbers
        // (`sdk/ui/src/protocol.ts`). A worker allowed to send more than the
        // host accepts turns a legitimate mount-time patch burst into a kill.
        let config = UiWorkerConfig::default();
        assert_eq!(
            config.message_rate_per_second,
            UI_WORKER_MESSAGE_RATE_PER_SECOND
        );
        assert_eq!(config.message_rate_burst, UI_WORKER_MESSAGE_BURST);
        assert_eq!(UI_WORKER_MESSAGE_RATE_PER_SECOND, 240);
        assert_eq!(UI_WORKER_MESSAGE_BURST, 120);
    }

    #[test]
    fn command_descriptors_use_the_command_invoke_capability() {
        assert_eq!(projection_capability("command"), Some("command-invoke"));
        assert_eq!(projection_capability("artifact"), Some("artifact-read"));
        // A workflow run's blackboard rides the workflow-read capability: same
        // resource id, same ownership join, read-only either way.
        assert_eq!(projection_capability("workflow"), Some("workflow-read"));
        assert_eq!(projection_capability("blackboard"), Some("workflow-read"));
        assert_eq!(projection_capability("raw-daemon-handle"), None);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_entrypoint_cannot_escape_verified_package_root() {
        use std::os::unix::fs::symlink;

        let (package, artifact) = package_with_worker(b"export {};");
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::remove_file(package.path().join("dist/worker.mjs")).unwrap();
        symlink(outside.path(), package.path().join("dist/worker.mjs")).unwrap();
        assert!(matches!(
            fixture_launch(
                &installed_ui(LifecycleState::Enabled, &artifact),
                &artifact,
                package.path(),
                UiTarget::Shared,
                UiWorkerLaunchPurpose::Active,
            ),
            Err(UiWorkerError::PackageVerification(_))
        ));
    }

    #[test]
    fn package_tree_must_exactly_match_the_verified_archive() {
        let (package, artifact) = package_with_worker(b"export {};");
        let installed = installed_ui(LifecycleState::Enabled, &artifact);
        std::fs::write(package.path().join("dist/injected.mjs"), "malicious();").unwrap();
        assert!(matches!(
            fixture_launch(
                &installed,
                &artifact,
                package.path(),
                UiTarget::Shared,
                UiWorkerLaunchPurpose::Active,
            ),
            Err(UiWorkerError::PackageVerification(_))
        ));
        std::fs::remove_file(package.path().join("dist/injected.mjs")).unwrap();
        std::fs::write(package.path().join("dist/worker.mjs"), "tampered();").unwrap();
        assert!(matches!(
            fixture_launch(
                &installed,
                &artifact,
                package.path(),
                UiTarget::Shared,
                UiWorkerLaunchPurpose::Active,
            ),
            Err(UiWorkerError::PackageVerification(_))
        ));
    }

    #[tokio::test]
    async fn package_is_revalidated_immediately_before_spawn() {
        let (package, artifact) = package_with_worker(b"export {};");
        let launch = fixture_launch(
            &installed_ui(LifecycleState::Enabled, &artifact),
            &artifact,
            package.path(),
            UiTarget::Shared,
            UiWorkerLaunchPurpose::Active,
        )
        .unwrap();
        std::fs::write(
            package.path().join("dist/worker.mjs"),
            "changed-after-seal();",
        )
        .unwrap();
        let supervisor = UiWorkerSupervisor::new(Arc::new(RefusingSandbox), test_config()).unwrap();
        assert!(matches!(
            supervisor.launch(launch, host_capabilities()).await,
            Err(UiWorkerError::PackageVerification(_))
        ));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn verified_fixture_completes_a_real_seatbelt_round_trip() {
        let Some(node) = test_node() else {
            return;
        };
        let fixture = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ui_worker.mjs"),
        )
        .unwrap();
        let (package, artifact) = package_with_worker(&fixture);
        let runtime_root = if node.starts_with("/opt/homebrew") {
            PathBuf::from("/opt/homebrew")
        } else if node.starts_with("/usr") {
            PathBuf::from("/usr")
        } else {
            node.parent()
                .and_then(Path::parent)
                .unwrap_or_else(|| node.parent().unwrap())
                .to_path_buf()
        };
        let runtime = UiWorkerRuntime::new(&node, runtime_root).unwrap();
        let launch = UiWorkerLaunch::from_installed_with_runtime(
            &installed_ui(LifecycleState::Enabled, &artifact),
            &artifact,
            package.path(),
            UiTarget::Shared,
            runtime,
            UiWorkerLaunchPurpose::Active,
        )
        .unwrap();
        let supervisor = match UiWorkerSupervisor::system(test_config()) {
            Ok(supervisor) => supervisor,
            Err(UiWorkerError::ResourceLauncherUnavailable) => return,
            Err(error) => panic!("Seatbelt setup failed: {error}"),
        };
        let mut worker = supervisor
            .launch(launch, host_capabilities())
            .await
            .expect("sandboxed fixture handshake");
        worker.shutdown().await.expect("sandboxed fixture shutdown");
    }
}
