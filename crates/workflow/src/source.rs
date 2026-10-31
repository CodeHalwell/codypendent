//! Resolving a workflow *by id* from its shipped, user, and repository sources
//! (STEP 5.1.4).
//!
//! [`crate::compile`] turns one manifest's text into a graph; this module answers
//! the prior question — *which* manifest text is the workflow named `id`? A named
//! workflow (the product path behind `/fix-ci`) is not shipped as a client-named
//! file; it is resolved from three sources, in ascending precedence:
//!
//! 1. **Built-in** ([`WorkflowScope::BuiltIn`]) — embedded in the binary, so a
//!    fresh install runs `/fix-ci` with no repository file. The canonical
//!    [`repair-github-check`](REPAIR_GITHUB_CHECK_ID) manifest is the only one
//!    today, included verbatim from `docs/specs/workflow.yaml` so the shipped
//!    definition and the spec of record can never drift.
//! 2. **User** ([`WorkflowScope::User`]) — a per-user directory of `*.yaml`
//!    manifests (the daemon points this at `<data_dir>/workflows`, mirroring the
//!    theme-pack data-dir convention).
//! 3. **Repository** ([`WorkflowScope::Repository`]) — `<repo>/.codypendent/workflows`,
//!    the same directory the TUI graph view already reads, so a project can shadow
//!    a built-in with its own definition.
//!
//! Two rules govern the set, both enforced at resolution:
//!
//! * **Version stability (the STEP 5.1.3 registry rule).** A published
//!   `(id, version)` is immutable: if the same id and version are declared with
//!   *different* content in two sources, that is a [`WorkflowSourceError::VersionCollision`]
//!   — "you changed a workflow without bumping its version". To change a workflow,
//!   bump its `version`.
//! * **Precedence / shadowing.** For a given id the effective definition is the
//!   one from the highest-precedence source (repository over user over built-in),
//!   breaking a same-scope tie by the higher version. A repository file therefore
//!   *shadows* the built-in — the clean way to shadow with changed content is to
//!   ship a bumped version (a same-version change would trip the collision rule
//!   above).
//!
//! The loader is resilient like the graph-view loader: a directory that does not
//! exist contributes nothing, and a file that cannot be read or parsed is skipped
//! (a broken sibling never hides a healthy workflow). It stays daemon- and
//! knowledge-free — the daemon supplies the two directory paths and compiles the
//! resolved text through the ordinary [`crate::compile_yaml`] path.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::model::{parse_definition, WorkflowDefinition, WorkflowInput};

/// Hash the parsed definition so comments and YAML formatting cannot invalidate a selection.
///
/// The serialized struct includes inputs, budgets, and every step. Its maps are
/// ordered, making the digest stable without retaining or exposing manifest text.
pub fn definition_digest(definition: &WorkflowDefinition) -> Result<String, serde_json::Error> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(definition)?)
    ))
}

/// The stable id of the canonical built-in workflow: investigate and repair a
/// failed GitHub check (the workflow `/fix-ci` runs).
pub const REPAIR_GITHUB_CHECK_ID: &str = "repair-github-check";

/// The canonical `repair-github-check` manifest, embedded so a fresh install can
/// run `/fix-ci` with no repository file. Included verbatim from the spec of
/// record (`docs/specs/workflow.yaml`) at build time, so the shipped built-in and
/// the spec are byte-for-byte identical and cannot drift.
pub const REPAIR_GITHUB_CHECK_MANIFEST: &str = include_str!("../../../docs/specs/workflow.yaml");

pub const PR_REVIEW_MANIFEST: &str = include_str!("../../../docs/specs/workflows/pr-review.yaml");
pub const SECURITY_SCAN_MANIFEST: &str =
    include_str!("../../../docs/specs/workflows/security-scan.yaml");
pub const DEPENDENCY_UPDATE_MANIFEST: &str =
    include_str!("../../../docs/specs/workflows/dependency-update.yaml");
pub const FAILING_CI_REPAIR_MANIFEST: &str =
    include_str!("../../../docs/specs/workflows/failing-ci-repair.yaml");
pub const STALE_DOCUMENT_REFRESH_MANIFEST: &str =
    include_str!("../../../docs/specs/workflows/stale-document-refresh.yaml");
pub const FLAKY_TEST_INVESTIGATION_MANIFEST: &str =
    include_str!("../../../docs/specs/workflows/flaky-test-investigation.yaml");
pub const REPOSITORY_HEALTH_REPORT_MANIFEST: &str =
    include_str!("../../../docs/specs/workflows/repository-health-report.yaml");
pub const RELEASE_PREPARATION_MANIFEST: &str =
    include_str!("../../../docs/specs/workflows/release-preparation.yaml");

pub const BUILTIN_WORKFLOW_MANIFESTS: &[&str] = &[
    REPAIR_GITHUB_CHECK_MANIFEST,
    PR_REVIEW_MANIFEST,
    SECURITY_SCAN_MANIFEST,
    DEPENDENCY_UPDATE_MANIFEST,
    FAILING_CI_REPAIR_MANIFEST,
    STALE_DOCUMENT_REFRESH_MANIFEST,
    FLAKY_TEST_INVESTIGATION_MANIFEST,
    REPOSITORY_HEALTH_REPORT_MANIFEST,
    RELEASE_PREPARATION_MANIFEST,
];

/// A workflow definition's provenance, ordered by ascending precedence
/// (`BuiltIn < User < Repository`), so a higher-precedence source shadows a lower
/// one for the same id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkflowScope {
    /// Embedded in the binary (lowest precedence).
    BuiltIn,
    /// The per-user workflows directory.
    User,
    /// The repository's `.codypendent/workflows` directory (highest precedence).
    Repository,
}

/// Maximum directory entries inspected per catalog source, including non-YAML files.
pub const MAX_CATALOG_DIRECTORY_ENTRIES: usize = 512;
/// Maximum manifests inspected across all catalog sources, including built-ins.
pub const MAX_CATALOG_MANIFESTS: usize = 256;
/// Maximum UTF-8 bytes read from one catalog manifest.
pub const MAX_CATALOG_MANIFEST_BYTES: usize = 256 * 1024;
/// Maximum manifest bytes read across the entire catalog.
pub const MAX_CATALOG_TOTAL_BYTES: usize = 2 * 1024 * 1024;
/// Maximum effective definitions returned in one catalog.
pub const MAX_CATALOG_ENTRIES: usize = 128;
/// Maximum bytes in a catalog workflow id, input name, or input type.
pub const MAX_CATALOG_IDENTIFIER_BYTES: usize = 128;
/// Maximum bytes in a catalog description; authoritative strings are never shortened.
pub const MAX_CATALOG_DESCRIPTION_BYTES: usize = 2048;
/// Maximum typed inputs exposed for one workflow.
pub const MAX_CATALOG_INPUTS: usize = 32;
/// Maximum compiled nodes in one catalog workflow.
pub const MAX_CATALOG_NODES: usize = 256;
/// Maximum distinct static diagnostic codes returned by a catalog.
pub const MAX_CATALOG_DIAGNOSTICS: usize = 16;

/// Bounded, non-executable metadata for the effective definition of one id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowCatalogEntry {
    pub id: String,
    pub version: u32,
    pub description: Option<String>,
    pub inputs: BTreeMap<String, WorkflowInput>,
    pub node_count: usize,
    pub source: WorkflowScope,
    /// Content precondition for starting this exact resolved definition.
    pub definition_digest: String,
}

/// Discovery result. Diagnostics are static codes, never parser errors or file contents.
///
/// `truncated` also signals an incompletely inspected source. In that case entries
/// are empty: an omitted manifest could shadow or collide with any visible id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowCatalog {
    pub entries: Vec<WorkflowCatalogEntry>,
    pub diagnostics: Vec<String>,
    pub truncated: bool,
}

impl WorkflowCatalog {
    fn diagnostic(&mut self, code: &'static str) {
        if self.diagnostics.iter().any(|existing| existing == code) {
            return;
        }
        if self.diagnostics.len() < MAX_CATALOG_DIAGNOSTICS {
            self.diagnostics.push(code.to_string());
        } else {
            self.truncated = true;
        }
    }

    fn incomplete(&mut self, code: &'static str) {
        self.truncated = true;
        self.diagnostic(code);
    }
}

/// A failure to resolve a workflow from its sources.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowSourceError {
    /// The same `(id, version)` was declared with different content in two
    /// sources — a published version was changed without a version bump.
    #[error(
        "workflow `{id}` version {version} is declared with different content in {first} and \
         {second}; a published workflow version is immutable — bump its `version` to change it"
    )]
    VersionCollision {
        /// The colliding workflow id.
        id: String,
        /// The version whose content diverged.
        version: u32,
        /// The first source declaring this `(id, version)`.
        first: String,
        /// The second source declaring it with different content.
        second: String,
    },
    /// No source defines a workflow with the requested id.
    #[error("no workflow named `{0}` is defined (built-in, user config, or repository)")]
    UnknownWorkflow(String),
}

impl WorkflowSourceError {
    /// The dotted error code the daemon surfaces to a client.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            WorkflowSourceError::VersionCollision { .. } => "workflow.version-collision",
            WorkflowSourceError::UnknownWorkflow(_) => "workflow.unknown-workflow",
        }
    }
}

/// One loaded workflow definition and where it came from.
#[derive(Debug, Clone)]
struct Candidate {
    scope: WorkflowScope,
    /// A human source label (`built-in`, or a file path) for error messages.
    source: String,
    definition: WorkflowDefinition,
    /// The manifest text verbatim, so the resolved workflow compiles through the
    /// ordinary [`crate::compile_yaml`] path with the author's exact bytes.
    raw: String,
}

/// The resolvable set of workflow definitions, gathered from the built-in(s), an
/// optional user directory, and an optional repository directory. Build it with
/// [`load`](WorkflowSourceRegistry::load); resolve a name with
/// [`resolve`](WorkflowSourceRegistry::resolve).
#[derive(Debug, Clone)]
pub struct WorkflowSourceRegistry {
    candidates: Vec<Candidate>,
}

impl WorkflowSourceRegistry {
    /// Enumerate effective metadata using a bounded loader independent of execution.
    ///
    /// Resolution still enforces the ordinary precedence and collision rules.
    /// Invalid YAML siblings are skipped, as execution does. Parsed candidates
    /// remain in resolution even when structurally invalid or too large to expose,
    /// so rejecting their metadata never silently selects a lower-precedence copy.
    /// Unreadable, unsafe, or incompletely scanned sources fail closed for discovery.
    #[must_use]
    pub fn catalog(user_dir: Option<&Path>, repository_dir: Option<&Path>) -> WorkflowCatalog {
        let mut catalog = WorkflowCatalog::default();
        let mut candidates = Vec::new();
        let mut manifests = 0;
        let mut total_bytes = 0;
        for raw in BUILTIN_WORKFLOW_MANIFESTS {
            catalog_candidate(
                raw,
                WorkflowScope::BuiltIn,
                &mut candidates,
                &mut catalog,
                &mut manifests,
                &mut total_bytes,
            );
        }
        for (directory, scope) in [
            (user_dir, WorkflowScope::User),
            (repository_dir, WorkflowScope::Repository),
        ] {
            if let Some(directory) = directory {
                load_catalog_dir(
                    directory,
                    scope,
                    &mut candidates,
                    &mut catalog,
                    &mut manifests,
                    &mut total_bytes,
                );
                if catalog.truncated {
                    return catalog;
                }
            }
        }
        if catalog.truncated {
            return catalog;
        }
        let registry = Self { candidates };
        let ids: BTreeSet<&str> = registry
            .candidates
            .iter()
            .map(|candidate| candidate.definition.id.as_str())
            .collect();
        for id in ids {
            let raw = match registry.resolve(id) {
                Ok(raw) => raw,
                Err(_) => {
                    catalog.diagnostic("workflow.version-collision");
                    continue;
                }
            };
            // Resolve chose these exact bytes. Identical copies still attribute
            // their metadata to the winning scope, rather than the first copy.
            let winner = registry
                .candidates
                .iter()
                .filter(|candidate| candidate.definition.id == id && candidate.raw == raw)
                .max_by_key(|candidate| (candidate.scope, candidate.definition.version))
                .expect("resolved manifest belongs to a loaded candidate");
            let definition = &winner.definition;
            if !catalog_metadata_valid(definition) {
                catalog.diagnostic("workflow.catalog-metadata-unavailable");
                continue;
            }
            let compiled = match crate::compile::compile(definition) {
                Ok(compiled) => compiled,
                Err(_) => {
                    catalog.diagnostic("workflow.catalog-invalid-manifest");
                    continue;
                }
            };
            if compiled.nodes.len() > MAX_CATALOG_NODES {
                catalog.diagnostic("workflow.catalog-metadata-unavailable");
                continue;
            }
            let definition_digest = match definition_digest(definition) {
                Ok(digest) => digest,
                Err(_) => {
                    catalog.diagnostic("workflow.catalog-metadata-unavailable");
                    continue;
                }
            };
            if catalog.entries.len() == MAX_CATALOG_ENTRIES {
                catalog.entries.clear();
                catalog.incomplete("workflow.catalog-limit");
                break;
            }
            catalog.entries.push(WorkflowCatalogEntry {
                id: definition.id.clone(),
                version: definition.version,
                description: definition.description.clone(),
                inputs: definition.inputs.clone(),
                node_count: compiled.nodes.len(),
                source: winner.scope,
                definition_digest,
            });
        }
        if catalog.truncated {
            catalog.entries.clear();
        }
        catalog
    }

    /// Gather the built-in workflow(s) plus every parseable `*.yaml` / `*.yml`
    /// manifest under the optional user and repository directories. A missing
    /// directory contributes nothing; an unreadable or unparseable file is
    /// skipped (a broken sibling never hides a healthy workflow), mirroring the
    /// graph-view loader.
    #[must_use]
    pub fn load(user_dir: Option<&Path>, repository_dir: Option<&Path>) -> Self {
        let mut candidates = Vec::new();
        // The built-ins are embedded and must always parse; if one somehow did not,
        // it simply contributes no candidate and the id resolves elsewhere or not
        // at all (a unit test pins that it parses).
        for manifest in BUILTIN_WORKFLOW_MANIFESTS {
            if let Ok(definition) = parse_definition(manifest) {
                candidates.push(Candidate {
                    scope: WorkflowScope::BuiltIn,
                    source: "built-in".to_string(),
                    definition,
                    raw: manifest.to_string(),
                });
            }
        }
        if let Some(dir) = user_dir {
            load_dir(dir, WorkflowScope::User, &mut candidates);
        }
        if let Some(dir) = repository_dir {
            load_dir(dir, WorkflowScope::Repository, &mut candidates);
        }
        Self { candidates }
    }

    /// Resolve a workflow id to its manifest text, enforcing version stability
    /// (a same-`(id, version)` content divergence is a
    /// [`VersionCollision`](WorkflowSourceError::VersionCollision)) and precedence
    /// (repository over user over built-in, then the higher version).
    pub fn resolve(&self, id: &str) -> Result<&str, WorkflowSourceError> {
        let matching: Vec<&Candidate> = self
            .candidates
            .iter()
            .filter(|candidate| candidate.definition.id == id)
            .collect();
        if matching.is_empty() {
            return Err(WorkflowSourceError::UnknownWorkflow(id.to_string()));
        }

        // Version stability: the same (id, version) must be byte-identical in
        // meaning everywhere it is declared. Compare parsed definitions so a
        // comment or whitespace change is not a false collision.
        for (i, a) in matching.iter().enumerate() {
            for b in &matching[i + 1..] {
                if a.definition.version == b.definition.version && a.definition != b.definition {
                    return Err(WorkflowSourceError::VersionCollision {
                        id: id.to_string(),
                        version: a.definition.version,
                        first: a.source.clone(),
                        second: b.source.clone(),
                    });
                }
            }
        }

        // Precedence: the highest scope wins; a same-scope tie (two files in one
        // directory declaring the same id at different versions) breaks to the
        // higher version. Collisions were already rejected, so any remaining tie
        // is between identical definitions and either is correct.
        let winner = matching
            .iter()
            .max_by(|a, b| {
                a.scope
                    .cmp(&b.scope)
                    .then(a.definition.version.cmp(&b.definition.version))
            })
            .expect("matching is non-empty");
        Ok(&winner.raw)
    }
}

fn catalog_metadata_valid(definition: &WorkflowDefinition) -> bool {
    let identifier = |value: &str| {
        !value.trim().is_empty()
            && value.len() <= MAX_CATALOG_IDENTIFIER_BYTES
            && !value.chars().any(char::is_control)
    };
    identifier(&definition.id)
        && definition
            .description
            .as_ref()
            .is_none_or(|description| description.len() <= MAX_CATALOG_DESCRIPTION_BYTES)
        && definition.inputs.len() <= MAX_CATALOG_INPUTS
        && definition.steps.len() <= MAX_CATALOG_NODES
        && definition
            .inputs
            .iter()
            .all(|(name, input)| identifier(name) && identifier(&input.input_type))
}

fn catalog_candidate(
    raw: &str,
    scope: WorkflowScope,
    out: &mut Vec<Candidate>,
    catalog: &mut WorkflowCatalog,
    manifests: &mut usize,
    total_bytes: &mut usize,
) {
    if *manifests >= MAX_CATALOG_MANIFESTS
        || raw.len() > MAX_CATALOG_MANIFEST_BYTES
        || raw.len() > MAX_CATALOG_TOTAL_BYTES.saturating_sub(*total_bytes)
    {
        catalog.incomplete("workflow.catalog-limit");
        return;
    }
    *manifests += 1;
    *total_bytes += raw.len();
    match parse_definition(raw) {
        Ok(definition) => out.push(Candidate {
            scope,
            // Catalog errors must not disclose filesystem paths or YAML scalars.
            source: "catalog source".to_string(),
            definition,
            raw: raw.to_string(),
        }),
        Err(_) => catalog.diagnostic("workflow.catalog-invalid-manifest"),
    }
}

fn load_catalog_dir(
    dir: &Path,
    scope: WorkflowScope,
    out: &mut Vec<Candidate>,
    catalog: &mut WorkflowCatalog,
    manifests: &mut usize,
    total_bytes: &mut usize,
) {
    let metadata = match std::fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            catalog.incomplete("workflow.catalog-source-unavailable");
            return;
        }
    };
    if metadata.is_symlink() || !metadata.is_dir() {
        catalog.incomplete("workflow.catalog-unsafe-source");
        return;
    }
    let root = match std::fs::canonicalize(dir) {
        Ok(root) => root,
        Err(_) => {
            catalog.incomplete("workflow.catalog-source-unavailable");
            return;
        }
    };
    // A symlinked product configuration parent must not turn discovery into
    // a read of another checkout's workflow directory.
    if scope == WorkflowScope::Repository
        && dir.file_name().is_some_and(|name| name == "workflows")
        && dir
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == ".codypendent" || name == ".son-of-anton")
    {
        let repository = dir
            .parent()
            .and_then(Path::parent)
            .and_then(|path| std::fs::canonicalize(path).ok());
        if repository.is_none_or(|repository| !root.starts_with(repository)) {
            catalog.incomplete("workflow.catalog-unsafe-source");
            return;
        }
    }
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(_) => {
            catalog.incomplete("workflow.catalog-source-unavailable");
            return;
        }
    };
    let mut files = Vec::new();
    for (count, entry) in entries.enumerate() {
        if count == MAX_CATALOG_DIRECTORY_ENTRIES {
            catalog.incomplete("workflow.catalog-limit");
            return;
        }
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(_) => {
                catalog.incomplete("workflow.catalog-source-unavailable");
                return;
            }
        };
        if matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("yaml" | "yml")
        ) {
            files.push(path);
        }
    }
    files.sort();
    for path in files {
        if *manifests >= MAX_CATALOG_MANIFESTS {
            catalog.incomplete("workflow.catalog-limit");
            return;
        }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                catalog.incomplete("workflow.catalog-source-unavailable");
                return;
            }
        };
        if metadata.is_symlink() || !metadata.is_file() {
            catalog.incomplete("workflow.catalog-unsafe-source");
            return;
        }
        if metadata.len() > MAX_CATALOG_MANIFEST_BYTES as u64
            || metadata.len() > MAX_CATALOG_TOTAL_BYTES.saturating_sub(*total_bytes) as u64
        {
            catalog.incomplete("workflow.catalog-limit");
            return;
        }
        let canonical = match std::fs::canonicalize(&path) {
            Ok(canonical) if canonical.starts_with(&root) => canonical,
            _ => {
                catalog.incomplete("workflow.catalog-unsafe-source");
                return;
            }
        };
        let bytes = std::fs::File::open(canonical).and_then(|file| {
            let mut bytes = Vec::new();
            let limit = (MAX_CATALOG_MANIFEST_BYTES + 1)
                .min(MAX_CATALOG_TOTAL_BYTES.saturating_sub(*total_bytes) + 1);
            file.take(limit as u64).read_to_end(&mut bytes)?;
            Ok(bytes)
        });
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(_) => {
                catalog.incomplete("workflow.catalog-source-unavailable");
                return;
            }
        };
        if bytes.len() > MAX_CATALOG_MANIFEST_BYTES
            || bytes.len() > MAX_CATALOG_TOTAL_BYTES.saturating_sub(*total_bytes)
        {
            catalog.incomplete("workflow.catalog-limit");
            return;
        }
        match std::str::from_utf8(&bytes) {
            Ok(raw) => catalog_candidate(raw, scope, out, catalog, manifests, total_bytes),
            Err(_) => {
                *manifests += 1;
                *total_bytes += bytes.len();
                catalog.diagnostic("workflow.catalog-invalid-manifest");
            }
        }
    }
}

/// Read every `*.yaml` / `*.yml` manifest in `dir` (sorted for determinism) and
/// push each parseable one as a [`Candidate`] at `scope`. A missing directory,
/// an unreadable file, or an unparseable manifest is skipped silently — a broken
/// file drops only itself, never a healthy sibling (the graph-view loader's
/// resilience). The `scope`-labelled source is the file path, so a collision
/// names the offending file.
fn load_dir(dir: &Path, scope: WorkflowScope, out: &mut Vec<Candidate>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("yaml" | "yml")
            )
        })
        .collect();
    files.sort();
    for path in files {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(definition) = parse_definition(&raw) {
            out.push(Candidate {
                scope,
                source: path.display().to_string(),
                definition,
                raw,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_yaml;

    fn catalog_fixture(id: &str, version: u32) -> String {
        format!(
            "schema_version: 1\nid: {id}\nversion: {version}\ndescription: Catalog fixture\n\
             inputs:\n  request:\n    type: string\n    required: true\n\
             steps:\n  - id: verify\n    tool: repository.test\n"
        )
    }

    #[test]
    fn definition_digest_ignores_comments_and_yaml_formatting() {
        let manifest = catalog_fixture("digest-fixture", 1);
        let reformatted = format!(
            "# A harmless comment\n{}\n# Another comment\n",
            manifest.replace("type: string", "type: 'string'")
        );
        let first = parse_definition(&manifest).unwrap();
        let second = parse_definition(&reformatted).unwrap();
        assert_eq!(
            definition_digest(&first).unwrap(),
            definition_digest(&second).unwrap()
        );
    }

    #[test]
    fn definition_digest_covers_input_budget_and_step_changes() {
        let definition = parse_definition(&catalog_fixture("digest-fixture", 1)).unwrap();
        let original = definition_digest(&definition).unwrap();
        let mut input = definition.clone();
        input.inputs.get_mut("request").unwrap().required = false;
        let mut budget = definition.clone();
        budget.budget.maximum_cost_usd = Some(1.0);
        let mut step = definition.clone();
        step.steps[0].tool = Some("workspace.read_file".to_string());
        for changed in [input, budget, step] {
            assert_ne!(original, definition_digest(&changed).unwrap());
        }
    }

    #[test]
    fn catalog_returns_sorted_builtin_metadata_and_compiled_node_counts() {
        let catalog = WorkflowSourceRegistry::catalog(None, None);
        assert!(!catalog.truncated);
        assert!(catalog.diagnostics.is_empty());
        assert_eq!(catalog.entries.len(), BUILTIN_WORKFLOW_MANIFESTS.len());
        assert!(catalog
            .entries
            .windows(2)
            .all(|pair| pair[0].id < pair[1].id));
        let repair = catalog
            .entries
            .iter()
            .find(|entry| entry.id == REPAIR_GITHUB_CHECK_ID)
            .unwrap();
        assert_eq!(
            (repair.version, repair.node_count, repair.source),
            (1, 5, WorkflowScope::BuiltIn)
        );
        assert_eq!(
            repair.inputs["pull_request"].input_type,
            "github_pull_request"
        );
        assert!(repair.inputs["pull_request"].required);
        let resolved = WorkflowSourceRegistry::load(None, None);
        let definition = parse_definition(resolved.resolve(&repair.id).unwrap()).unwrap();
        assert_eq!(
            repair.definition_digest,
            definition_digest(&definition).unwrap()
        );
    }

    #[test]
    fn catalog_preserves_scope_precedence_and_same_scope_highest_version() {
        let user = tempfile::tempdir().unwrap();
        let repository = tempfile::tempdir().unwrap();
        std::fs::write(
            user.path().join("a.yaml"),
            catalog_fixture("catalog-demo", 8),
        )
        .unwrap();
        std::fs::write(
            user.path().join("b.yml"),
            catalog_fixture("catalog-demo", 9),
        )
        .unwrap();
        let user_catalog = WorkflowSourceRegistry::catalog(Some(user.path()), None);
        let user_entry = user_catalog
            .entries
            .iter()
            .find(|entry| entry.id == "catalog-demo")
            .unwrap();
        assert_eq!(
            (user_entry.version, user_entry.source),
            (9, WorkflowScope::User)
        );
        std::fs::write(
            repository.path().join("r.yaml"),
            catalog_fixture("catalog-demo", 2),
        )
        .unwrap();
        let catalog = WorkflowSourceRegistry::catalog(Some(user.path()), Some(repository.path()));
        let entry = catalog
            .entries
            .iter()
            .find(|entry| entry.id == "catalog-demo")
            .unwrap();
        assert_eq!(
            (entry.version, entry.node_count, entry.source),
            (2, 1, WorkflowScope::Repository)
        );
    }

    #[test]
    fn catalog_reports_collisions_but_identical_redeclarations_keep_winner_scope() {
        let user = tempfile::tempdir().unwrap();
        let repository = tempfile::tempdir().unwrap();
        let manifest = catalog_fixture("catalog-demo", 1);
        std::fs::write(user.path().join("a.yaml"), &manifest).unwrap();
        std::fs::write(repository.path().join("r.yaml"), &manifest).unwrap();
        let identical = WorkflowSourceRegistry::catalog(Some(user.path()), Some(repository.path()));
        assert_eq!(
            identical
                .entries
                .iter()
                .find(|entry| entry.id == "catalog-demo")
                .unwrap()
                .source,
            WorkflowScope::Repository
        );
        std::fs::write(
            repository.path().join("r.yaml"),
            manifest.replace("Catalog fixture", "Changed definition"),
        )
        .unwrap();
        let collision = WorkflowSourceRegistry::catalog(Some(user.path()), Some(repository.path()));
        assert!(!collision
            .entries
            .iter()
            .any(|entry| entry.id == "catalog-demo"));
        assert_eq!(collision.diagnostics, ["workflow.version-collision"]);
    }

    #[test]
    fn catalog_keeps_healthy_siblings_without_disclosing_invalid_yaml() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("healthy.yaml"),
            catalog_fixture("catalog-demo", 1),
        )
        .unwrap();
        std::fs::write(
            directory.path().join("sensitive-file-name.yaml"),
            "password: sensitive-yaml-scalar\ninvalid: [",
        )
        .unwrap();
        let catalog = WorkflowSourceRegistry::catalog(None, Some(directory.path()));
        assert!(!catalog.truncated);
        assert!(catalog
            .entries
            .iter()
            .any(|entry| entry.id == "catalog-demo"));
        assert_eq!(catalog.diagnostics, ["workflow.catalog-invalid-manifest"]);
        assert!(!format!("{catalog:?}").contains("sensitive"));
    }

    #[test]
    fn catalog_does_not_fall_back_past_a_parsed_but_invalid_winner() {
        let user = tempfile::tempdir().unwrap();
        let repository = tempfile::tempdir().unwrap();
        std::fs::write(
            user.path().join("a.yaml"),
            catalog_fixture("catalog-demo", 1),
        )
        .unwrap();
        std::fs::write(
            repository.path().join("r.yaml"),
            catalog_fixture("catalog-demo", 2).replace("schema_version: 1", "schema_version: 999"),
        )
        .unwrap();
        let catalog = WorkflowSourceRegistry::catalog(Some(user.path()), Some(repository.path()));
        assert!(!catalog
            .entries
            .iter()
            .any(|entry| entry.id == "catalog-demo"));
        assert_eq!(catalog.diagnostics, ["workflow.catalog-invalid-manifest"]);
    }

    #[test]
    fn catalog_rejects_oversized_metadata_without_shortening_identifiers() {
        let directory = tempfile::tempdir().unwrap();
        let fixtures = [
            catalog_fixture(&"i".repeat(MAX_CATALOG_IDENTIFIER_BYTES + 1), 1),
            catalog_fixture("large-description", 1).replace(
                "Catalog fixture",
                &"d".repeat(MAX_CATALOG_DESCRIPTION_BYTES + 1),
            ),
            catalog_fixture("large-input-name", 1).replace(
                "  request:",
                &format!("  {}:", "n".repeat(MAX_CATALOG_IDENTIFIER_BYTES + 1)),
            ),
            catalog_fixture("large-input-type", 1).replace(
                "type: string",
                &format!("type: {}", "t".repeat(MAX_CATALOG_IDENTIFIER_BYTES + 1)),
            ),
        ];
        for (index, fixture) in fixtures.iter().enumerate() {
            std::fs::write(directory.path().join(format!("{index}.yaml")), fixture).unwrap();
        }
        let catalog = WorkflowSourceRegistry::catalog(None, Some(directory.path()));
        assert_eq!(catalog.entries.len(), BUILTIN_WORKFLOW_MANIFESTS.len());
        assert_eq!(
            catalog.diagnostics,
            ["workflow.catalog-metadata-unavailable"]
        );
    }

    #[test]
    fn catalog_bounds_input_and_node_counts() {
        let directory = tempfile::tempdir().unwrap();
        let inputs: String = (0..=MAX_CATALOG_INPUTS)
            .map(|index| format!("  input_{index}:\n    type: string\n"))
            .collect();
        let nodes: String = (0..=MAX_CATALOG_NODES)
            .map(|index| format!("  - id: step_{index}\n    tool: repository.test\n"))
            .collect();
        std::fs::write(directory.path().join("inputs.yaml"), format!("schema_version: 1\nid: many-inputs\nversion: 1\ninputs:\n{inputs}steps:\n  - id: verify\n    tool: repository.test\n")).unwrap();
        std::fs::write(
            directory.path().join("nodes.yaml"),
            format!("schema_version: 1\nid: many-nodes\nversion: 1\nsteps:\n{nodes}"),
        )
        .unwrap();
        let catalog = WorkflowSourceRegistry::catalog(None, Some(directory.path()));
        assert_eq!(catalog.entries.len(), BUILTIN_WORKFLOW_MANIFESTS.len());
        assert_eq!(
            catalog.diagnostics,
            ["workflow.catalog-metadata-unavailable"]
        );
    }

    #[test]
    fn catalog_limits_never_advertise_an_unverified_fallback() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("oversized.yaml"),
            "#".repeat(MAX_CATALOG_MANIFEST_BYTES + 1),
        )
        .unwrap();
        let catalog = WorkflowSourceRegistry::catalog(None, Some(directory.path()));
        assert!(catalog.truncated);
        assert!(catalog.entries.is_empty());
        assert_eq!(catalog.diagnostics, ["workflow.catalog-limit"]);
    }

    #[test]
    fn catalog_bounds_directory_enumeration_and_output_entries() {
        let directory = tempfile::tempdir().unwrap();
        for index in 0..=MAX_CATALOG_DIRECTORY_ENTRIES {
            std::fs::write(directory.path().join(format!("{index}.txt")), "ignored").unwrap();
        }
        let catalog = WorkflowSourceRegistry::catalog(Some(directory.path()), None);
        assert!(catalog.truncated && catalog.entries.is_empty());
        let directory = tempfile::tempdir().unwrap();
        for index in 0..=MAX_CATALOG_ENTRIES {
            std::fs::write(
                directory.path().join(format!("{index}.yaml")),
                catalog_fixture(&format!("entry-{index}"), 1),
            )
            .unwrap();
        }
        let catalog = WorkflowSourceRegistry::catalog(Some(directory.path()), None);
        assert!(catalog.truncated && catalog.entries.is_empty());
        assert_eq!(catalog.diagnostics, ["workflow.catalog-limit"]);
    }

    #[test]
    fn catalog_bounds_manifest_count_and_cumulative_read_bytes() {
        let directory = tempfile::tempdir().unwrap();
        for index in 0..=MAX_CATALOG_MANIFESTS {
            std::fs::write(
                directory.path().join(format!("{index:04}.yaml")),
                "not a workflow",
            )
            .unwrap();
        }
        let catalog = WorkflowSourceRegistry::catalog(Some(directory.path()), None);
        assert!(catalog.truncated && catalog.entries.is_empty());
        assert!(catalog
            .diagnostics
            .iter()
            .any(|code| code == "workflow.catalog-limit"));

        let directory = tempfile::tempdir().unwrap();
        let raw = format!("#{}\n", "a".repeat(MAX_CATALOG_MANIFEST_BYTES - 2));
        for index in 0..=MAX_CATALOG_TOTAL_BYTES / MAX_CATALOG_MANIFEST_BYTES {
            std::fs::write(directory.path().join(format!("{index:04}.yaml")), &raw).unwrap();
        }
        let catalog = WorkflowSourceRegistry::catalog(Some(directory.path()), None);
        assert!(catalog.truncated && catalog.entries.is_empty());
        assert!(catalog
            .diagnostics
            .iter()
            .any(|code| code == "workflow.catalog-limit"));
    }

    #[cfg(unix)]
    #[test]
    fn catalog_refuses_external_file_and_parent_directory_symlinks() {
        use std::os::unix::fs::symlink;
        let external = tempfile::tempdir().unwrap();
        std::fs::create_dir(external.path().join("workflows")).unwrap();
        let manifest = external.path().join("workflows/private.yaml");
        std::fs::write(&manifest, catalog_fixture("external-workflow", 1)).unwrap();
        let directory = tempfile::tempdir().unwrap();
        symlink(&manifest, directory.path().join("linked.yaml")).unwrap();
        let catalog = WorkflowSourceRegistry::catalog(None, Some(directory.path()));
        assert!(catalog.truncated && catalog.entries.is_empty());
        assert_eq!(catalog.diagnostics, ["workflow.catalog-unsafe-source"]);
        let repository = tempfile::tempdir().unwrap();
        symlink(external.path(), repository.path().join(".codypendent")).unwrap();
        let catalog = WorkflowSourceRegistry::catalog(
            None,
            Some(&repository.path().join(".codypendent/workflows")),
        );
        assert!(catalog.truncated && catalog.entries.is_empty());
        assert_eq!(catalog.diagnostics, ["workflow.catalog-unsafe-source"]);
    }

    /// The embedded built-in parses and is the canonical five-node
    /// `repair-github-check` graph — a fresh install ships a working `/fix-ci`.
    #[test]
    fn the_built_in_repair_workflow_ships_and_compiles() {
        let registry = WorkflowSourceRegistry::load(None, None);
        let manifest = registry.resolve(REPAIR_GITHUB_CHECK_ID).unwrap();
        let compiled = compile_yaml(manifest).expect("built-in compiles");
        assert_eq!(compiled.id, REPAIR_GITHUB_CHECK_ID);
        assert_eq!(compiled.version, 1);
        let order: Vec<&str> = compiled.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(order, ["inspect", "patch", "verify", "review", "publish"]);
    }

    /// An unknown id is a legible error, not a panic or an empty resolve.
    #[test]
    fn an_unknown_workflow_is_a_legible_error() {
        let registry = WorkflowSourceRegistry::load(None, None);
        let error = registry.resolve("no-such-workflow").unwrap_err();
        assert_eq!(error.code(), "workflow.unknown-workflow");
    }

    /// A repository file declaring the built-in's `(id, version)` with DIFFERENT
    /// content is the "changed the YAML without bumping the version" error.
    #[test]
    fn a_same_version_content_change_is_a_collision() {
        let dir = tempfile::tempdir().unwrap();
        // Same id + version 1 as the built-in, but a changed budget — a real
        // content divergence at an unchanged version.
        std::fs::write(
            dir.path().join("repair.yaml"),
            REPAIR_GITHUB_CHECK_MANIFEST.replace("maximum_agents: 2", "maximum_agents: 3"),
        )
        .unwrap();
        let registry = WorkflowSourceRegistry::load(None, Some(dir.path()));
        let error = registry.resolve(REPAIR_GITHUB_CHECK_ID).unwrap_err();
        assert_eq!(error.code(), "workflow.version-collision");
        match error {
            WorkflowSourceError::VersionCollision { id, version, .. } => {
                assert_eq!(id, REPAIR_GITHUB_CHECK_ID);
                assert_eq!(version, 1);
            }
            other => panic!("expected a version collision, got {other:?}"),
        }
    }

    /// A byte-identical repository copy at the same `(id, version)` is NOT a
    /// collision — it shadows the built-in harmlessly (idempotent redeclaration).
    #[test]
    fn an_identical_redeclaration_is_not_a_collision() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("repair.yaml"), REPAIR_GITHUB_CHECK_MANIFEST).unwrap();
        let registry = WorkflowSourceRegistry::load(None, Some(dir.path()));
        let manifest = registry.resolve(REPAIR_GITHUB_CHECK_ID).unwrap();
        assert_eq!(compile_yaml(manifest).unwrap().version, 1);
    }

    /// A repository file with a BUMPED version shadows the built-in: resolving the
    /// id returns the repository's manifest, not the built-in's.
    #[test]
    fn a_repository_file_shadows_the_built_in_by_bumping_the_version() {
        let dir = tempfile::tempdir().unwrap();
        // A bumped version (2) is the clean override — a new immutable identity, so
        // no collision with the built-in's v1.
        let shadow = REPAIR_GITHUB_CHECK_MANIFEST.replace("\nversion: 1", "\nversion: 2");
        std::fs::write(dir.path().join("repair.yaml"), &shadow).unwrap();
        let registry = WorkflowSourceRegistry::load(None, Some(dir.path()));
        let manifest = registry.resolve(REPAIR_GITHUB_CHECK_ID).unwrap();
        let compiled = compile_yaml(manifest).unwrap();
        assert_eq!(compiled.version, 2, "the repository's bumped version wins");
        assert_eq!(
            manifest, shadow,
            "the resolved text is the repository file's, not the built-in's"
        );
    }

    /// Repository precedence beats the user directory for the same id.
    #[test]
    fn repository_scope_outranks_the_user_directory() {
        let user = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let user_def = REPAIR_GITHUB_CHECK_MANIFEST.replace("\nversion: 1", "\nversion: 2");
        let repo_def = REPAIR_GITHUB_CHECK_MANIFEST.replace("\nversion: 1", "\nversion: 3");
        std::fs::write(user.path().join("r.yaml"), &user_def).unwrap();
        std::fs::write(repo.path().join("r.yaml"), &repo_def).unwrap();
        let registry = WorkflowSourceRegistry::load(Some(user.path()), Some(repo.path()));
        let manifest = registry.resolve(REPAIR_GITHUB_CHECK_ID).unwrap();
        assert_eq!(
            compile_yaml(manifest).unwrap().version,
            3,
            "repository (v3) outranks user (v2) and built-in (v1)"
        );
    }

    /// A missing directory and an unparseable sibling never hide the built-in.
    #[test]
    fn broken_or_missing_sources_fall_back_to_the_built_in() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.yaml"), "this: is: not: a: workflow").unwrap();
        // A never-created user directory + a repo directory with only a broken file.
        let registry =
            WorkflowSourceRegistry::load(Some(Path::new("/no/such/dir")), Some(dir.path()));
        let manifest = registry.resolve(REPAIR_GITHUB_CHECK_ID).unwrap();
        assert_eq!(compile_yaml(manifest).unwrap().version, 1);
    }

    #[test]
    fn all_builtin_manifests_parse_and_compile() {
        let registry = WorkflowSourceRegistry::load(None, None);
        for manifest in BUILTIN_WORKFLOW_MANIFESTS {
            let def = parse_definition(manifest).expect("built-in manifest must parse");
            let resolved = registry
                .resolve(&def.id)
                .expect("built-in manifest must resolve");
            let compiled = compile_yaml(resolved).expect("built-in manifest must compile");
            assert_eq!(compiled.id, def.id);
            assert_eq!(compiled.version, def.version);
        }
    }
}
