//! Resolving a workflow's agent roles to loaded agent profiles (STEP 5.1).
//!
//! A workflow step names a short agent *role* (`implementer`); execution needs
//! the full [`AgentProfile`] behind it — its mode, autonomy, model policy,
//! skills, tools, permissions, and budget. [`AgentProfileSet`] is the resolution
//! layer between the two: it loads a directory of `agent.toml` profiles and
//! indexes them by the role each [fulfils](AgentProfile::fulfilled_role), so a
//! manifest's `role: implementer` binds to the `code.implementer` profile.
//!
//! Resolution is deterministic and unambiguous: exactly one profile may fulfil a
//! role, so [`load_dir`](AgentProfileSet::load_dir) refuses a directory where two
//! profiles claim the same role rather than binding a role to whichever file
//! happened to load last. The set stays daemon-free — it is the roles half of the
//! [`WorkflowRegistry`](crate::registry::WorkflowRegistry) seam the daemon fills,
//! and [`unresolved_roles`](AgentProfileSet::unresolved_roles) is the cross-check
//! a `workflow validate` runs so an author learns a role has no profile before a
//! run ever reaches it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::agent::{parse_agent_profile, AgentProfile, AgentProfileError};
use crate::compile::{CompiledWorkflow, NodeAction};

/// A failure to load a set of agent profiles from a directory.
#[derive(Debug, thiserror::Error)]
pub enum AgentProfileSetError {
    /// The profile directory could not be read (missing, not a directory, …).
    #[error("reading agent-profile directory {dir}: {source}")]
    ReadDir {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A profile file could not be read.
    #[error("reading agent profile {path}: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A profile file did not parse / validate.
    #[error("{path}: {source}")]
    Profile {
        path: PathBuf,
        #[source]
        source: AgentProfileError,
    },
    /// Two profiles fulfil the same role — a role must resolve to exactly one
    /// profile, so the set refuses rather than bind to an arbitrary one.
    #[error(
        "agent role `{role}` is fulfilled by two profiles ({first} and {second}); \
         a role must resolve to exactly one profile"
    )]
    AmbiguousRole {
        role: String,
        first: String,
        second: String,
    },
}

/// A role a workflow references that no loaded profile fulfils.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedRole {
    /// The step (node) whose agent names the role.
    pub step: String,
    /// The unresolved role.
    pub role: String,
}

/// A set of agent profiles indexed by the role each fulfils. Built from a
/// directory of `agent.toml` files; the daemon builds the same map from its
/// loaded profiles to answer role lookups.
#[derive(Debug, Clone, Default)]
pub struct AgentProfileSet {
    by_role: BTreeMap<String, AgentProfile>,
}

impl AgentProfileSet {
    /// An empty set — every role is unresolved until profiles are added.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load every `*.toml` profile in `dir`, indexed by the role each fulfils.
    ///
    /// Files are read in sorted order so an ambiguous-role error names the same
    /// pair deterministically. A read or parse failure fails the whole load
    /// (a partially-loaded set would silently drop a role a workflow needs); two
    /// profiles fulfilling one role is an [`AmbiguousRole`](AgentProfileSetError::AmbiguousRole)
    /// error. Non-`.toml` files are ignored.
    pub fn load_dir(dir: &Path) -> Result<Self, AgentProfileSetError> {
        let read = std::fs::read_dir(dir).map_err(|source| AgentProfileSetError::ReadDir {
            dir: dir.to_path_buf(),
            source,
        })?;
        let mut files: Vec<PathBuf> = read
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"))
            .collect();
        files.sort();

        let mut set = Self::new();
        for path in files {
            let toml = std::fs::read_to_string(&path).map_err(|source| {
                AgentProfileSetError::ReadFile {
                    path: path.clone(),
                    source,
                }
            })?;
            let profile = parse_agent_profile(&toml)
                .map_err(|source| AgentProfileSetError::Profile { path, source })?;
            set.insert(profile)?;
        }
        Ok(set)
    }

    /// Add one profile, keyed by the role it fulfils. Errors if another profile
    /// already fulfils that role.
    pub fn insert(&mut self, profile: AgentProfile) -> Result<(), AgentProfileSetError> {
        let role = profile.fulfilled_role().to_owned();
        if let Some(existing) = self.by_role.get(&role) {
            return Err(AgentProfileSetError::AmbiguousRole {
                role,
                first: existing.id.clone(),
                second: profile.id.clone(),
            });
        }
        self.by_role.insert(role, profile);
        Ok(())
    }

    /// The profile fulfilling `role`, if one is loaded.
    #[must_use]
    pub fn resolve(&self, role: &str) -> Option<&AgentProfile> {
        self.by_role.get(role)
    }

    /// Whether a profile fulfils `role`.
    #[must_use]
    pub fn has_role(&self, role: &str) -> bool {
        self.by_role.contains_key(role)
    }

    /// The number of loaded profiles (one per fulfilled role).
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_role.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_role.is_empty()
    }

    /// The roles this set fulfils, sorted.
    pub fn roles(&self) -> impl Iterator<Item = &str> {
        self.by_role.keys().map(String::as_str)
    }

    /// Every agent node in `compiled` whose role no loaded profile fulfils, in
    /// topological order. Empty means every agent role resolves — the check a
    /// `workflow validate --agents` reports so an author fixes an unresolved role
    /// before the workflow runs. Tool nodes carry no role and are never returned.
    #[must_use]
    pub fn unresolved_roles(&self, compiled: &CompiledWorkflow) -> Vec<UnresolvedRole> {
        compiled
            .nodes
            .iter()
            .filter_map(|node| match &node.action {
                NodeAction::Agent { role, .. } if !self.has_role(role) => Some(UnresolvedRole {
                    step: node.id.clone(),
                    role: role.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile_yaml;

    const MANIFEST: &str = "\
schema_version: 1
id: repair-github-check
version: 1
orchestration_reason: independent-review
budget:
  maximum_agents: 2
steps:
  - id: inspect
    agent:
      role: investigator
    outputs: [finding]
  - id: patch
    depends_on: [inspect]
    agent:
      role: implementer
    outputs: [proposed_patch]
  - id: verify
    depends_on: [patch]
    tool: repository.test
    outputs: [test_result]
";

    fn write_profile(dir: &Path, file: &str, id: &str, extra: &str) {
        let toml = format!("schema_version = 1\nid = \"{id}\"\nname = \"{id}\"\n{extra}");
        std::fs::write(dir.join(file), toml).unwrap();
    }

    #[test]
    fn load_dir_indexes_profiles_by_fulfilled_role() {
        let tmp = tempfile::tempdir().unwrap();
        write_profile(tmp.path(), "impl.toml", "code.implementer", "");
        // An explicit role that differs from the id suffix.
        write_profile(
            tmp.path(),
            "inv.toml",
            "agents.scout",
            "role = \"investigator\"\n",
        );
        // A non-profile file is ignored.
        std::fs::write(tmp.path().join("notes.md"), "ignore").unwrap();

        let set = AgentProfileSet::load_dir(tmp.path()).unwrap();
        assert_eq!(set.len(), 2);
        assert!(set.has_role("implementer"));
        assert_eq!(set.resolve("implementer").unwrap().id, "code.implementer");
        assert_eq!(set.resolve("investigator").unwrap().id, "agents.scout");
        assert!(!set.has_role("reviewer"));
    }

    #[test]
    fn two_profiles_for_one_role_is_ambiguous() {
        let tmp = tempfile::tempdir().unwrap();
        write_profile(tmp.path(), "a.toml", "code.reviewer", "");
        write_profile(tmp.path(), "b.toml", "security.reviewer", "");
        match AgentProfileSet::load_dir(tmp.path()) {
            Err(AgentProfileSetError::AmbiguousRole { role, .. }) => assert_eq!(role, "reviewer"),
            other => panic!("expected an ambiguous-role error, got {other:?}"),
        }
    }

    #[test]
    fn unresolved_roles_lists_agent_steps_without_a_profile() {
        let compiled = compile_yaml(MANIFEST).unwrap();

        // Only `implementer` is loaded: `investigator` is unresolved; the tool
        // step `verify` is never reported (it carries no role).
        let mut set = AgentProfileSet::new();
        write_profile_into(&mut set, "code.implementer");
        let unresolved = set.unresolved_roles(&compiled);
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].step, "inspect");
        assert_eq!(unresolved[0].role, "investigator");

        // With both agent roles loaded, nothing is unresolved.
        write_profile_into(&mut set, "agents.investigator");
        assert!(set.unresolved_roles(&compiled).is_empty());
    }

    fn write_profile_into(set: &mut AgentProfileSet, id: &str) {
        let toml = format!("schema_version = 1\nid = \"{id}\"\nname = \"{id}\"\n");
        set.insert(parse_agent_profile(&toml).unwrap()).unwrap();
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(matches!(
            AgentProfileSet::load_dir(&missing),
            Err(AgentProfileSetError::ReadDir { .. })
        ));
    }
}
