//! The skill-package loader (Chapter 05, STEP 2.2).
//!
//! A skill is a directory (`SKILL.md`, `skill.toml`, optional `tests/`,
//! `references/`, `scripts/`, `assets/`). [`load_package`] parses its
//! `skill.toml` with **exactly** the key shapes of [`specs/skill.toml`], rejects
//! packages that declare an entrypoint that is not on disk or a scope that does
//! not match the tier it is being registered under, content-hashes every file in
//! the directory, and folds the result into a [`RegistryItem`] the registry can
//! store.
//!
//! Two rules from the spec are enforced structurally here:
//! - **Unknown keys are rejected.** Every manifest struct carries
//!   `#[serde(deny_unknown_fields)]`, so a stray top-level or nested key fails to
//!   parse rather than being silently ignored.
//! - **Scripts are not runnable in Phase 2.** A package that ships a non-empty
//!   `scripts/` entrypoint is marked [`executable = false`](RegistryItem::executable)
//!   so retrieval never selects a script-dependent behaviour before the Phase-6
//!   sandbox exists.
//!
//! [`specs/skill.toml`]: ../../../docs/specs/skill.toml

use std::path::Path;

use chrono::Utc;
use codypendent_protocol::RegistryItemId;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::types::{
    CapabilityRequest, Provenance, RegistryDependency, RegistryItem, RegistryItemKind,
    RegistryStatus, RiskClass, Scope, TrustMetadata, TrustTier, Version,
};

/// The parsed `skill.toml`. Field names and types mirror
/// [`specs/skill.toml`](../../../docs/specs/skill.toml) exactly; unknown keys are
/// rejected so a typo never disappears into a silently-ignored field.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillManifest {
    /// Manifest format version (currently `1`).
    pub schema_version: u32,
    /// The stable identity slug, e.g. `"rust.fix-ci"` — this becomes the registry
    /// item's [`name`](RegistryItem::name), the value shadowing and dependency
    /// references resolve against.
    pub id: String,
    /// The human-readable display title, e.g. `"Fix Rust CI"`.
    pub name: String,
    /// Semantic version string (`MAJOR.MINOR.PATCH`).
    pub version: String,
    /// The scope tier the skill targets (`"repository"`, `"user"`, …); must equal
    /// the tier of the [`Scope`] it is registered under.
    pub scope: String,
    /// Lifecycle status (`draft | active | modified | deprecated`).
    pub status: String,
    /// One-line summary shown on the skill's card.
    pub description: String,
    /// Task intents the skill answers, for retrieval.
    #[serde(default)]
    pub intents: Vec<String>,
    /// Languages the skill applies to (kept as keywords).
    #[serde(default)]
    pub languages: Vec<String>,
    /// Tools the skill needs — hard dependencies.
    #[serde(default)]
    pub required_tools: Vec<String>,
    /// Tools the skill can use if present — soft dependencies.
    #[serde(default)]
    pub optional_tools: Vec<String>,
    /// The `[permissions]` table, flattened into [`CapabilityRequest`]s.
    #[serde(default)]
    pub permissions: SkillPermissions,
    /// The `[limits]` table. Parsed to validate the manifest; **not persisted** —
    /// budget enforcement is Phase 5, so nothing downstream reads it yet.
    #[serde(default)]
    pub limits: SkillLimits,
    /// The `[entrypoints]` table naming the package's files/dirs.
    #[serde(default)]
    pub entrypoints: SkillEntrypoints,
    /// The `[trust]` table (publisher + signature policy).
    pub trust: SkillTrust,
}

/// The `[permissions]` table — each field a list of capability targets.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillPermissions {
    #[serde(default)]
    pub filesystem_read: Vec<String>,
    #[serde(default)]
    pub filesystem_write: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub secrets: Vec<String>,
}

/// The `[limits]` table. Parsed for validation only (see [`SkillManifest::limits`]);
/// enforcement of iteration / duration / cost ceilings lands in Phase 5.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillLimits {
    pub maximum_iterations: Option<u32>,
    pub maximum_duration_seconds: Option<u64>,
    pub maximum_cost_usd: Option<f64>,
}

/// The `[entrypoints]` table. Every declared path must exist on disk under the
/// package directory, or [`load_package`] rejects the package.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillEntrypoints {
    /// The instructions file (`SKILL.md`).
    pub instructions: Option<String>,
    /// The tests directory.
    pub tests: Option<String>,
    /// The references directory.
    pub references: Option<String>,
    /// The scripts directory (recorded but not runnable until Phase 6).
    pub scripts: Option<String>,
}

impl SkillEntrypoints {
    /// Every declared entrypoint path, in declaration order.
    fn declared(&self) -> impl Iterator<Item = &String> {
        [
            self.instructions.as_ref(),
            self.tests.as_ref(),
            self.references.as_ref(),
            self.scripts.as_ref(),
        ]
        .into_iter()
        .flatten()
    }
}

/// The `[trust]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillTrust {
    /// Publisher identity. The reserved value `"local-user"` maps to
    /// [`TrustTier::FirstParty`]; anything else is [`TrustTier::Community`].
    pub publisher: String,
    /// Whether a signature is required before the item may run.
    #[serde(default)]
    pub signature_required: bool,
}

/// Publisher value that marks a package as locally authored (first-party trust).
const LOCAL_PUBLISHER: &str = "local-user";

/// A failure loading or validating a skill package.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// Reading `skill.toml` or walking the package directory failed.
    #[error("reading skill package: {0}")]
    Io(#[from] std::io::Error),
    /// `skill.toml` did not parse — a syntax error, a missing required key, or an
    /// **unknown key** (rejected by `deny_unknown_fields`).
    #[error("parsing skill.toml: {0}")]
    Toml(#[from] toml::de::Error),
    /// A declared entrypoint path does not exist under the package directory.
    #[error("declared entrypoint `{path}` does not exist under the package directory")]
    MissingEntrypoint { path: String },

    #[error("declared entrypoint `{path}` escapes the package directory")]
    EscapingEntrypoint { path: String },
    /// The manifest's `scope` string does not match the tier of the [`Scope`] the
    /// package is being registered under.
    #[error("manifest scope `{declared}` does not match the registration scope `{expected}`")]
    ScopeMismatch { declared: String, expected: String },
    /// The `version` string is not a plain `MAJOR.MINOR.PATCH`.
    #[error("invalid version `{0}` (expected MAJOR.MINOR.PATCH)")]
    InvalidVersion(String),
    /// The `status` string is not one of `draft | active | modified | deprecated`.
    #[error("unknown status `{0}` (expected draft|active|modified|deprecated)")]
    UnknownStatus(String),
}

/// Load and validate the skill package at `dir`, folding it into a
/// [`RegistryItem`] registered under `scope`.
///
/// Validation, in order: the manifest parses (unknown keys rejected); every
/// declared entrypoint exists on disk under `dir`; the manifest `scope` string
/// equals `scope.tier()`; the `version` is well-formed; the `status` is known.
/// The item's `content_hash` is taken over **all** files in `dir` (recursively,
/// path-sorted for determinism), so any later file change without a version bump
/// is detectable. `[permissions]` is flattened into [`CapabilityRequest`]s and
/// the item's [`RiskClass`] derived from them. A non-empty `scripts/` entrypoint
/// marks the item non-[`executable`](RegistryItem::executable).
pub fn load_package(dir: &Path, scope: Scope) -> Result<RegistryItem, ManifestError> {
    let raw = std::fs::read_to_string(dir.join("skill.toml"))?;
    let manifest: SkillManifest = toml::from_str(&raw)?;

    // Every declared entrypoint must exist AND stay within the package. A `../`
    // or absolute entrypoint could otherwise validate — and later silently
    // change — a file outside `dir` that `hash_package` never hashes (so the
    // change would go undetected as `Modified`).
    let package_root = dir.canonicalize()?;
    for path in manifest.entrypoints.declared() {
        let Ok(resolved) = dir.join(path).canonicalize() else {
            return Err(ManifestError::MissingEntrypoint { path: path.clone() });
        };
        if !resolved.starts_with(&package_root) {
            return Err(ManifestError::EscapingEntrypoint { path: path.clone() });
        }
    }

    // The manifest's tier must match the scope it is being registered under.
    if manifest.scope != scope.tier() {
        return Err(ManifestError::ScopeMismatch {
            declared: manifest.scope.clone(),
            expected: scope.tier().to_string(),
        });
    }

    let version = Version(manifest.version.clone());
    if !version.is_valid() {
        return Err(ManifestError::InvalidVersion(manifest.version.clone()));
    }
    let status = parse_status(&manifest.status)?;

    let content_hash = hash_package(dir)?;

    let permissions = flatten_permissions(&manifest.permissions);
    let risk = RiskClass::from_permissions(&permissions);

    // Scripts are recorded but not runnable until the Phase-6 sandbox: a package
    // that ships a non-empty scripts/ dir is not executable, so retrieval can
    // never select a script-dependent behaviour.
    let executable = !scripts_present(dir, &manifest.entrypoints)?;

    let tier = if manifest.trust.publisher == LOCAL_PUBLISHER {
        TrustTier::FirstParty
    } else {
        TrustTier::Community
    };
    let trust = TrustMetadata {
        publisher: manifest.trust.publisher.clone(),
        signature_required: manifest.trust.signature_required,
        signature: None,
        tier,
    };

    // Required tools are hard dependencies; optional tools are soft.
    let dependencies = manifest
        .required_tools
        .iter()
        .map(|target| RegistryDependency {
            target: target.clone(),
            optional: false,
        })
        .chain(
            manifest
                .optional_tools
                .iter()
                .map(|target| RegistryDependency {
                    target: target.clone(),
                    optional: true,
                }),
        )
        .collect();

    // Languages plus the human title are kept as lexical keywords (RegistryItem
    // has no separate display-title field; `name` carries the stable id).
    let mut keywords = manifest.languages.clone();
    keywords.push(manifest.name.clone());

    let now = Utc::now();
    Ok(RegistryItem {
        id: RegistryItemId::new(),
        kind: RegistryItemKind::Skill,
        name: manifest.id.clone(),
        version,
        scope,
        description: manifest.description.clone(),
        intents: manifest.intents.clone(),
        keywords,
        examples: Vec::new(),
        input_schema: None,
        output_schema: None,
        dependencies,
        permissions,
        risk,
        provenance: Provenance::Package {
            path: dir.display().to_string(),
        },
        trust,
        status,
        content_hash,
        executable,
        created_at: now,
        updated_at: now,
    })
}

/// Flatten a `[permissions]` table into the registry's capability list.
fn flatten_permissions(permissions: &SkillPermissions) -> Vec<CapabilityRequest> {
    let mut out = Vec::new();
    out.extend(
        permissions
            .filesystem_read
            .iter()
            .cloned()
            .map(CapabilityRequest::FilesystemRead),
    );
    out.extend(
        permissions
            .filesystem_write
            .iter()
            .cloned()
            .map(CapabilityRequest::FilesystemWrite),
    );
    out.extend(
        permissions
            .commands
            .iter()
            .cloned()
            .map(CapabilityRequest::Command),
    );
    out.extend(
        permissions
            .network
            .iter()
            .cloned()
            .map(CapabilityRequest::Network),
    );
    out.extend(
        permissions
            .secrets
            .iter()
            .cloned()
            .map(CapabilityRequest::Secret),
    );
    out
}

/// Whether the package declares a `scripts` entrypoint that exists and is a
/// non-empty directory.
fn scripts_present(dir: &Path, entrypoints: &SkillEntrypoints) -> Result<bool, ManifestError> {
    let Some(scripts) = &entrypoints.scripts else {
        return Ok(false);
    };
    let scripts_dir = dir.join(scripts);
    if !scripts_dir.is_dir() {
        return Ok(false);
    }
    Ok(std::fs::read_dir(&scripts_dir)?.next().is_some())
}

/// Map the manifest `status` string to a [`RegistryStatus`].
fn parse_status(status: &str) -> Result<RegistryStatus, ManifestError> {
    match status {
        "draft" => Ok(RegistryStatus::Draft),
        "active" => Ok(RegistryStatus::Active),
        "modified" => Ok(RegistryStatus::Modified),
        "deprecated" => Ok(RegistryStatus::Deprecated),
        other => Err(ManifestError::UnknownStatus(other.to_string())),
    }
}

/// Content-hash every file in the package directory.
///
/// Walks `dir` recursively, sorts files by their normalized relative path (so the
/// digest is independent of directory-read order and platform separators), and
/// folds each path and its bytes — length-prefixed so no path/content boundary is
/// ambiguous — into one SHA-256, hex-encoded. Any file added, removed, or edited
/// changes the digest.
fn hash_package(dir: &Path) -> Result<String, ManifestError> {
    let mut files = Vec::new();
    collect_files(dir, dir, &mut files)?;
    files.sort_by(|(a, _), (b, _)| a.cmp(b));

    let mut hasher = Sha256::new();
    for (relative, bytes) in files {
        hasher.update((relative.len() as u64).to_le_bytes());
        hasher.update(relative.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Recursively gather `(normalized-relative-path, bytes)` for every regular file
/// under `dir`.
fn collect_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), ManifestError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(root, &path, out)?;
        } else if file_type.is_file() {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((relative, std::fs::read(&path)?));
        }
    }
    Ok(())
}
