//! Capabilities and the scopes they carry (STEP 1.5).
//!
//! A [`Capability`] is the unit a run is granted before a tool executes. The
//! Phase 1 subset covers filesystem reads/writes, command execution, network
//! connections, and Git commit/push. Each scope is *checked after
//! canonicalization*: a path is resolved (its `..` segments and symlinks
//! collapsed) before it is compared against an allowed root, so neither
//! traversal nor a planted symlink can smuggle a path out of scope. Deny always
//! wins over allow, even inside an allowed root.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The default fallback for a network destination that is not on the allow
/// list. `deny` is the Phase 1 built-in default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkDefault {
    /// Permit destinations not otherwise listed.
    Allow,
    /// Reject destinations not otherwise listed.
    Deny,
}

/// A time-limited, invocation-scoped capability. The Phase 1 subset of the
/// [Chapter 11](../../docs/docs/11-security-and-governance.md) capability model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Capability {
    /// Read files within a [`PathScope`].
    FileRead(PathScope),
    /// Write files within a [`PathScope`].
    FileWrite(PathScope),
    /// Execute an allow-listed program within a [`CommandScope`].
    CommandExecute(CommandScope),
    /// Open a network connection permitted by a [`NetworkScope`].
    NetworkConnect(NetworkScope),
    /// Create a Git commit in the run's repository.
    GitCommit,
    /// Push to a Git remote.
    GitPush,
}

/// The verdict of checking a single path against a [`PathScope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeVerdict {
    /// Inside an allowed root and not denied.
    Allowed,
    /// Not under any allowed root.
    OutsideRoots,
    /// Matched the deny list (deny wins even inside an allowed root).
    Denied,
}

/// A set of canonical allowed root directories plus a canonical deny list.
///
/// The `roots` and `deny` paths are already canonicalized (built at evaluation
/// time from the merged policy with `$REPOSITORY`/`$WORKTREE`/`$HOME`
/// expanded). A candidate path is canonicalized on the fly in [`classify`]
/// before comparison.
///
/// [`classify`]: PathScope::classify
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathScope {
    /// Canonical directories a path must fall under to be in scope.
    pub roots: Vec<PathBuf>,
    /// Canonical directories that are denied even inside an allowed root.
    pub deny: Vec<PathBuf>,
}

impl PathScope {
    /// Build a scope from already-canonical roots and deny entries.
    pub fn new(roots: Vec<PathBuf>, deny: Vec<PathBuf>) -> Self {
        Self { roots, deny }
    }

    /// Canonicalize `path` and classify it against this scope. Deny wins: a
    /// path under a deny entry is [`ScopeVerdict::Denied`] even when it is also
    /// under an allowed root.
    pub fn classify(&self, path: &Path) -> ScopeVerdict {
        let canonical = canonicalize_lenient(path);
        if self.deny.iter().any(|d| is_within(&canonical, d)) {
            return ScopeVerdict::Denied;
        }
        if self.roots.iter().any(|r| is_within(&canonical, r)) {
            ScopeVerdict::Allowed
        } else {
            ScopeVerdict::OutsideRoots
        }
    }

    /// Whether `path` is allowed by this scope (convenience over [`classify`]).
    ///
    /// [`classify`]: PathScope::classify
    pub fn allows(&self, path: &Path) -> bool {
        matches!(self.classify(path), ScopeVerdict::Allowed)
    }
}

/// The programs a run may execute and the wall-clock ceiling for each.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandScope {
    /// Executables permitted by name (bare name or a matching leaf of a full
    /// path).
    pub allowed_programs: Vec<String>,
    /// Maximum wall-clock seconds a single command may run.
    pub maximum_seconds: u64,
}

impl CommandScope {
    /// Whether `program` is allow-listed. Matches an exact entry, or an entry
    /// equal to the final path component of `program` (so both `cargo` and
    /// `/usr/bin/cargo` match an allow-list entry of `cargo`).
    pub fn allows_program(&self, program: &str) -> bool {
        let leaf = Path::new(program).file_name();
        self.allowed_programs
            .iter()
            .any(|p| p == program || leaf.is_some_and(|name| name == std::ffi::OsStr::new(p)))
    }
}

/// The network destinations a run may reach and the fallback for the rest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkScope {
    /// Explicitly permitted `host:port` destinations.
    pub allow: Vec<String>,
    /// What to do with a destination not on `allow`.
    pub default: NetworkDefault,
}

impl NetworkScope {
    /// Whether `destination` (a `host:port` string) may be reached.
    pub fn allows(&self, destination: &str) -> bool {
        if self.allow.iter().any(|d| d == destination) {
            return true;
        }
        matches!(self.default, NetworkDefault::Allow)
    }
}

/// Canonicalize `path`, resolving `..` and symlinks. When the full path does
/// not exist, canonicalize the nearest existing ancestor (which resolves any
/// symlinks and `..` in the existing prefix) and re-append the remainder,
/// collapsing `.`/`..` in that remainder lexically. This lets a not-yet-created
/// leaf still be checked against a scope while a symlinked or `..`-laden prefix
/// is fully resolved first.
pub(crate) fn canonicalize_lenient(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }
    let mut existing = path;
    while let Some(parent) = existing.parent() {
        if let Ok(base) = std::fs::canonicalize(parent) {
            let remainder = path.strip_prefix(parent).unwrap_or_else(|_| Path::new(""));
            let mut result = base;
            for component in remainder.components() {
                match component {
                    Component::ParentDir => {
                        result.pop();
                    }
                    Component::Normal(segment) => result.push(segment),
                    Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
                }
            }
            return result;
        }
        existing = parent;
    }
    path.to_path_buf()
}

/// Component-wise containment: `candidate` is `root` or lives under it. Uses
/// path components (never raw string prefixes) so `/foobar` is not "under"
/// `/foo`.
pub(crate) fn is_within(candidate: &Path, root: &Path) -> bool {
    candidate == root || candidate.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    #[test]
    fn is_within_is_component_wise() {
        assert!(is_within(Path::new("/foo/bar"), Path::new("/foo")));
        assert!(is_within(Path::new("/foo"), Path::new("/foo")));
        assert!(!is_within(Path::new("/foobar"), Path::new("/foo")));
        assert!(!is_within(Path::new("/foo"), Path::new("/foo/bar")));
    }

    #[test]
    fn lenient_resolves_parent_dir_in_missing_tail() {
        let dir = tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        // `<root>/exists/nope/../other` — only `<root>/exists` exists.
        std::fs::create_dir(root.join("exists")).unwrap();
        let messy = root.join("exists/nope/../other");
        let resolved = canonicalize_lenient(&messy);
        assert_eq!(resolved, root.join("exists/other"));
    }

    #[test]
    fn lenient_resolves_symlink_prefix() {
        let dir = tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();
        let link = root.join("link");
        symlink(&real, &link).unwrap();
        // A not-yet-created file under the symlink resolves through it.
        let resolved = canonicalize_lenient(&link.join("new.txt"));
        assert_eq!(resolved, real.join("new.txt"));
    }

    #[test]
    fn command_scope_matches_bare_and_full_path() {
        let scope = CommandScope {
            allowed_programs: vec!["cargo".to_string()],
            maximum_seconds: 900,
        };
        assert!(scope.allows_program("cargo"));
        assert!(scope.allows_program("/usr/bin/cargo"));
        assert!(!scope.allows_program("rm"));
    }
}
