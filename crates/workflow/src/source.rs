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

use std::path::{Path, PathBuf};

use crate::model::{parse_definition, WorkflowDefinition};

/// The stable id of the canonical built-in workflow: investigate and repair a
/// failed GitHub check (the workflow `/fix-ci` runs).
pub const REPAIR_GITHUB_CHECK_ID: &str = "repair-github-check";

/// The canonical `repair-github-check` manifest, embedded so a fresh install can
/// run `/fix-ci` with no repository file. Included verbatim from the spec of
/// record (`docs/specs/workflow.yaml`) at build time, so the shipped built-in and
/// the spec are byte-for-byte identical and cannot drift.
pub const REPAIR_GITHUB_CHECK_MANIFEST: &str = include_str!("../../../docs/specs/workflow.yaml");

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
    /// Gather the built-in workflow(s) plus every parseable `*.yaml` / `*.yml`
    /// manifest under the optional user and repository directories. A missing
    /// directory contributes nothing; an unreadable or unparseable file is
    /// skipped (a broken sibling never hides a healthy workflow), mirroring the
    /// graph-view loader.
    #[must_use]
    pub fn load(user_dir: Option<&Path>, repository_dir: Option<&Path>) -> Self {
        let mut candidates = Vec::new();
        // The built-in is embedded and must always parse; if it somehow did not,
        // it simply contributes no candidate and the id resolves elsewhere or not
        // at all (a unit test pins that it parses).
        if let Ok(definition) = parse_definition(REPAIR_GITHUB_CHECK_MANIFEST) {
            candidates.push(Candidate {
                scope: WorkflowScope::BuiltIn,
                source: "built-in".to_string(),
                definition,
                raw: REPAIR_GITHUB_CHECK_MANIFEST.to_string(),
            });
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
}
