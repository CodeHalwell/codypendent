//! Which repository the desktop client is working in.
//!
//! The shell had no answer to this before: `daemon_connect` sent
//! `std::env::current_dir()` as the `repository` on every `CreateSession`,
//! `AttachSession` and `StartRun`. For a bundled `.app` that is the launch
//! working directory — `/` under Finder, `$HOME` under a login shell — and the
//! daemon indexes whatever it is handed.
//!
//! # The incident this module exists to prevent
//!
//! A daemon once indexed `$HOME`, reaching a 510,904-node code graph that was
//! 76% IDE cache, with every run spending 30-60s in `Preparing`. The hole was
//! `git rev-parse --show-toplevel`: with `GIT_DIR` inherited from the
//! environment (a git hook, `git rebase -x`, `git bisect run`, an exporting
//! shell) it does not answer "is this a repository" — it answers with the
//! *current directory*, exit 0. `crates/codypendentd/src/scan.rs` fixed that by
//! stripping the repository-location variables from the child environment, and
//! `crates/daemon/src/server.rs::plausible_repository_root` added a second,
//! independent gate that is a pure FILESYSTEM check and never shells out at
//! all.
//!
//! A selection must pass BOTH gates:
//!
//! 1. [`crate::repo_anchor::checkout_root`] — `git rev-parse --show-toplevel`
//!    with those eight variables stripped from the child environment. That call
//!    is NOT repeated here: this crate answers "where does the checkout start"
//!    in exactly one place, so the variable list cannot drift between two
//!    copies and silently re-open the hole it closes. A host with no `git` at
//!    all falls back to [`filesystem_checkout_root`], a pure ancestor walk that
//!    nothing in the environment can redirect.
//! 2. [`plausible_repository_root`] — refuse `$HOME`, refuse a path with fewer
//!    than two named components, and require a `.git` entry at the resolved
//!    root or an ancestor. A FILESYSTEM check with no `git` in it, so a defect
//!    in the question above cannot open the same hole twice.
//!
//! A folder that fails either is REFUSED with the reason. There is no silent
//! fallback to the working directory: that fallback is the defect.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{AgentMode, WorkspaceId};
use serde::{Deserialize, Serialize};

/// The shell's own preferences file. Not the daemon's, not the TUI's: this
/// records only what the desktop window chose, and today that is one field.
const PREFERENCES_FILE: &str = "desktop.json";

/// A repository the desktop client may work in: the git checkout root, exactly
/// as the daemon's own resolver would anchor it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositorySelection {
    /// The canonicalized checkout root. This is the string that rides on
    /// `CreateSession.repository`, `AttachSession.repository` and
    /// `StartRun.repository`, and the path a council run is anchored to.
    pub path: String,
    /// The last path component, for a compact label. Never used as an identity.
    pub name: String,
    /// The directory the operator actually chose, when it was a subdirectory of
    /// the checkout. Shown so "I picked `repo/src`, it says `repo`" is
    /// explained rather than surprising; `None` when they picked the root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picked: Option<String>,
}

impl RepositorySelection {
    fn new(root: PathBuf, picked: &Path) -> Self {
        let name = root.file_name().map_or_else(
            || root.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let picked_display = picked.display().to_string();
        let path = root.display().to_string();
        let picked = (picked_display != path).then_some(picked_display);
        Self { path, name, picked }
    }
}

/// What the desktop persists between launches.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Preferences {
    /// The chosen checkout root, or absent when nothing has been chosen. Absent
    /// is a real state the UI renders as "no repository selected" — it is never
    /// filled in with a guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repository: Option<String>,
    /// The mode and model staged for the next run. These used to live only in
    /// the shell's memory: the Models page said "used from now on" and the
    /// choice was gone at the next launch, while the repository beside it was
    /// remembered. Absent fields are "not chosen", never a guess.
    #[serde(default)]
    run_defaults: StoredRunDefaults,
    /// The workspace this shell identifies as, minted once and kept.
    ///
    /// It used to be minted fresh inside every `DaemonClient::connect`, which
    /// meant every automatic reconnect adopted a NEW workspace while the app
    /// re-attached the SAME session — so workspace-scoped memories and
    /// documents vanished from Memory and Docs after any socket drop, until
    /// something restored a matching scope. A workspace is an identity, not a
    /// connection attribute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
}

/// The persisted half of the shell's `RunDefaults`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredRunDefaults {
    /// Serialized in the protocol enum's own `{ "type": "Build" }` shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<AgentMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// The staged run defaults from the last launch, if any were saved.
pub fn stored_run_defaults() -> anyhow::Result<StoredRunDefaults> {
    let paths = crate::runtime_paths::resolve().context("resolving the codypendent data dir")?;
    Ok(load_preferences(&paths)?.run_defaults)
}

/// Persist the staged run defaults beside the repository selection.
pub fn store_run_defaults(defaults: &StoredRunDefaults) -> anyhow::Result<()> {
    update_preferences(|preferences| preferences.run_defaults = defaults.clone())
}

/// The workspace this shell identifies as, minted on first use and persisted.
///
/// Stable across reconnects AND across launches: the knowledge scope a person
/// sees must not depend on how many times the socket dropped. A stored value
/// that no longer parses is replaced rather than propagated.
/// The workspace used when preferences cannot be persisted, minted ONCE for the
/// life of the process.
static FALLBACK_WORKSPACE: std::sync::OnceLock<WorkspaceId> = std::sync::OnceLock::new();

/// The persisted workspace, or a process-stable stand-in when preferences are
/// unreadable or unwritable.
///
/// Falling back to `None` let the daemon mint a fresh workspace on EVERY
/// connection, so an operator whose `desktop.json` was corrupt or on a
/// read-only volume watched workspace-scoped memories and documents disappear
/// after each automatic reconnect — the session reattached, but its knowledge
/// scope moved. This cannot survive a restart, because nothing can if the file
/// will not write, but it holds for the life of the process, which is what a
/// reconnect needs.
pub fn workspace_for_connection() -> WorkspaceId {
    stable_workspace().unwrap_or_else(|_| *FALLBACK_WORKSPACE.get_or_init(WorkspaceId::new))
}

pub fn stable_workspace() -> anyhow::Result<WorkspaceId> {
    // Read AND mint under the lock: two connections racing here would
    // otherwise each mint an id and one would be overwritten, which is the
    // unstable-workspace bug by another route.
    //
    // A stored id is returned WITHOUT writing. Saving unconditionally made
    // every read depend on the write succeeding, so on read-only storage a
    // perfectly readable identity was thrown away for a fresh process
    // fallback — reintroducing, from the other side, the vanishing-knowledge
    // bug this function exists to prevent.
    with_preferences(|preferences| {
        if let Some(stored) = preferences
            .workspace
            .as_deref()
            .and_then(|text| text.parse::<WorkspaceId>().ok())
        {
            return (Save::No, stored);
        }
        let minted = WorkspaceId::new();
        preferences.workspace = Some(minted.to_string());
        (Save::Yes, minted)
    })
}

/// Serializes every read-modify-write of `desktop.json`.
///
/// Each mutation loads the whole file, changes ONE field, and renames a full
/// snapshot back. Two overlapping Tauri commands — pinning a model while a
/// repository selection is still saving — therefore both read the same old
/// state, and whichever renamed last erased the other's field while both
/// reported success. The atomic rename prevents torn JSON; it does not prevent
/// a lost update.
static PREFERENCES_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Whether a critical section actually changed anything worth persisting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Save {
    Yes,
    No,
}

/// Load, mutate and save `desktop.json` as ONE critical section, writing only
/// when the closure says it changed something.
///
/// A poisoned lock is recovered rather than propagated: a panic in some other
/// mutation must not leave the shell unable to remember anything again.
fn with_preferences<T>(mutate: impl FnOnce(&mut Preferences) -> (Save, T)) -> anyhow::Result<T> {
    let paths = crate::runtime_paths::resolve().context("resolving the codypendent data dir")?;
    let _guard = PREFERENCES_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut preferences = load_preferences(&paths)?;
    let (save, outcome) = mutate(&mut preferences);
    if save == Save::Yes {
        save_preferences(&paths, &preferences)?;
    }
    Ok(outcome)
}

/// [`with_preferences`] for a caller that always writes.
fn update_preferences<T>(mutate: impl FnOnce(&mut Preferences) -> T) -> anyhow::Result<T> {
    with_preferences(|preferences| (Save::Yes, mutate(preferences)))
}

fn preferences_path(paths: &RuntimePaths) -> PathBuf {
    paths.config_dir.join(PREFERENCES_FILE)
}

fn load_preferences(paths: &RuntimePaths) -> anyhow::Result<Preferences> {
    let path = preferences_path(paths);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
        }
        // A missing file is "nothing chosen yet". A CORRUPT one is an error the
        // caller must see, exactly as `AuthStore::load` distinguishes the two —
        // silently treating a damaged preferences file as empty would discard a
        // selection the operator made and quietly re-point the daemon.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Preferences::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn save_preferences(paths: &RuntimePaths, preferences: &Preferences) -> anyhow::Result<()> {
    let path = preferences_path(paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(preferences)?;
    // A temp name unique to this write, not a fixed `desktop.json.tmp`.
    //
    // Two saves racing on one shared temp path interleave their bytes into the
    // same file and then both rename it over `desktop.json`, so the surviving
    // preferences file can be a mixture of the two — or truncated JSON, which
    // loads as "no repository selected" and silently discards the operator's
    // choice. `AuthStore::save` already avoids this by putting the pid in the
    // temp name; the pid alone is not enough here, because these saves are
    // Tauri commands and two of them can be in flight inside one process, so a
    // per-process counter goes in beside it.
    static WRITE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = WRITE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = path.with_extension(format!("{}.{nonce}.json.tmp", std::process::id()));
    std::fs::write(&temporary, text.as_bytes())
        .with_context(|| format!("writing {}", temporary.display()))?;
    // Rename is atomic within the directory, so a concurrent save either wins
    // or loses outright; neither can be seen half-applied by a reader.
    std::fs::rename(&temporary, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// The nearest ancestor of `dir` (inclusive) holding a `.git` entry.
///
/// The fallback for a host with no `git` binary, and deliberately a pure
/// filesystem walk: nothing in the environment can redirect it.
fn filesystem_checkout_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(Path::to_path_buf)
}

/// How many *named* components a path has — the prefix and root separator do
/// not count, so `/` is 0, `/Users` is 1, `/Users/dan` is 2.
fn named_depth(path: &Path) -> usize {
    path.components()
        .filter(|component| matches!(component, std::path::Component::Normal(_)))
        .count()
}

/// Whether `root` is plausibly a repository checkout worth indexing.
///
/// Ported from `crates/daemon/src/server.rs::plausible_repository_root`, and
/// deliberately a FILESYSTEM check with no `git` invocation in it: it is the
/// second, independent gate, so a defect in the git question cannot open the
/// same hole twice.
fn plausible_repository_root(root: &Path) -> bool {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    for key in ["HOME", "USERPROFILE"] {
        if let Ok(home) = std::env::var(key) {
            let home = PathBuf::from(home);
            let home = home.canonicalize().unwrap_or(home);
            if canonical == home {
                return false;
            }
        }
    }
    // `/`, `/Users`, `/home` and friends are never a checkout.
    if named_depth(&canonical) < 2 {
        return false;
    }
    canonical.ancestors().any(|dir| dir.join(".git").exists())
}

/// Validate an operator-chosen directory and anchor it to its checkout root.
///
/// Every refusal names its reason. There is deliberately no branch that returns
/// a fallback path: a folder that is not a checkout, or that is the home
/// directory, is an error the operator sees, not a silently-substituted value
/// the daemon then walks.
pub fn validate_repository(chosen: &Path) -> anyhow::Result<RepositorySelection> {
    let canonical = chosen
        .canonicalize()
        .with_context(|| format!("resolving {}", chosen.display()))?;
    if !canonical.is_dir() {
        bail!("{} is not a directory", canonical.display());
    }

    // One git question, asked in `repo_anchor` with the ambient repository
    // variables stripped; the filesystem walk covers a host with no `git`.
    let Some(root) = crate::repo_anchor::checkout_root(&canonical)
        .or_else(|| filesystem_checkout_root(&canonical))
    else {
        bail!(
            "{} is not a git checkout. Codypendent indexes a repository's code graph, so it \
             needs the working tree of a repository — choose a folder that contains a `.git` \
             entry, or one inside it.",
            canonical.display()
        );
    };
    let root = root.canonicalize().unwrap_or(root);

    // The second gate. `plausible_repository_root` covers the home directory
    // and account roots as well as the `.git` requirement, so this rejects the
    // exact shape of the 510,904-node incident even if git answered `Some`.
    if !plausible_repository_root(&root) {
        if is_home_directory(&root) {
            bail!(
                "{} is your home directory. Indexing it once produced a 510,904-node code \
                 graph that was 76% editor cache, so it is refused: choose the repository \
                 checkout you want to work in.",
                root.display()
            );
        }
        bail!(
            "{} is not a repository checkout Codypendent will index (it is too close to the \
             filesystem root, or holds no `.git`).",
            root.display()
        );
    }

    Ok(RepositorySelection::new(root, &canonical))
}

fn is_home_directory(path: &Path) -> bool {
    ["HOME", "USERPROFILE"].iter().any(|key| {
        std::env::var(key).is_ok_and(|home| {
            let home = PathBuf::from(home);
            let home = home.canonicalize().unwrap_or(home);
            home == path
        })
    })
}

/// The repository the operator selected, or `None` when they have not selected
/// one. `None` is a real answer the UI renders as such — it is never a cue to
/// substitute the process working directory.
pub fn selected_repository() -> anyhow::Result<Option<RepositorySelection>> {
    let paths = crate::runtime_paths::resolve().context("resolving codypendent runtime paths")?;
    let Some(stored) = load_preferences(&paths)?.repository else {
        return Ok(None);
    };
    // Re-validate on read. A checkout that has been moved or deleted since the
    // selection was made must surface as an error, not keep being sent to the
    // daemon as a repository that no longer exists.
    validate_repository(Path::new(&stored)).map(Some)
}

/// Persist `chosen` as the repository, after validating it.
pub fn select_repository(chosen: &Path) -> anyhow::Result<RepositorySelection> {
    let selection = validate_repository(chosen)?;
    update_preferences(|preferences| preferences.repository = Some(selection.path.clone()))?;
    Ok(selection)
}

/// Forget the selection. The client then has no repository until one is chosen.
pub fn clear_repository() -> anyhow::Result<()> {
    update_preferences(|preferences| preferences.repository = None)
}

/// The `repository` string a new connection should carry.
///
/// The stored selection when there is a valid one. Otherwise `None` — NOT the
/// process working directory, which for a bundled `.app` is the launch
/// directory and was the path by which `$HOME` reached the indexer.
pub fn connection_repository() -> Option<String> {
    match selected_repository() {
        Ok(selection) => selection.map(|selection| selection.path),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_in(dir: &Path) -> RuntimePaths {
        RuntimePaths {
            data_dir: dir.to_path_buf(),
            config_dir: dir.to_path_buf(),
            run_dir: dir.to_path_buf(),
            socket_path: dir.join("sock"),
            pid_path: dir.join("pid"),
            log_dir: dir.to_path_buf(),
        }
    }

    /// Concurrent saves must not be able to shred the preferences file.
    ///
    /// Every save used the same `desktop.json.tmp`. Two of them in flight wrote
    /// their bytes into that one file and then both renamed it into place, so
    /// the survivor could be a mixture of the two or truncated — and truncated
    /// JSON loads as "no repository selected", quietly discarding the
    /// operator's chosen checkout. These are Tauri commands, so the two writers
    /// can be threads of one process.
    ///
    /// The file must parse after every save, whatever the interleaving, and
    /// must name one of the repositories actually written — never a blend.
    #[test]
    fn concurrent_preference_saves_cannot_produce_a_torn_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_in(dir.path());
        let written: Vec<String> = (0..8).map(|i| format!("/repo/{i}")).collect();

        std::thread::scope(|scope| {
            for repository in &written {
                let paths = &paths;
                let written = &written;
                scope.spawn(move || {
                    for _ in 0..40 {
                        save_preferences(
                            paths,
                            &Preferences {
                                repository: Some(repository.clone()),
                                run_defaults: StoredRunDefaults::default(),
                                workspace: None,
                            },
                        )
                        .expect("save");
                        // Read back mid-storm: any torn write is visible here.
                        let loaded = load_preferences(paths).expect("load");
                        let seen = loaded.repository.expect("a repository was saved");
                        assert!(
                            written.contains(&seen),
                            "loaded a repository nobody wrote: {seen}"
                        );
                    }
                });
            }
        });

        // No temp files survive a clean run.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    /// Reading a stored workspace must not depend on the write succeeding.
    ///
    /// `save_preferences` renames a temp file over the target, so a write
    /// REPLACES the inode. An unchanged inode is therefore proof no write
    /// happened — which is what makes the read work on read-only storage,
    /// where insisting on a write discarded a readable identity and handed
    /// back a fresh one instead.
    #[cfg(unix)]
    #[test]
    fn reading_a_stored_workspace_writes_nothing() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.config_dir).expect("config dir");
        let stored = WorkspaceId::new();
        save_preferences(
            &paths,
            &Preferences {
                repository: None,
                run_defaults: StoredRunDefaults::default(),
                workspace: Some(stored.to_string()),
            },
        )
        .expect("seed");
        let before = std::fs::metadata(preferences_path(&paths))
            .expect("metadata")
            .ino();

        // The read half of `stable_workspace`, at this test's paths.
        let read = || {
            let _guard = PREFERENCES_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut preferences = load_preferences(&paths).expect("load");
            let (save, id) = if let Some(found) = preferences
                .workspace
                .as_deref()
                .and_then(|text| text.parse::<WorkspaceId>().ok())
            {
                (Save::No, found)
            } else {
                let minted = WorkspaceId::new();
                preferences.workspace = Some(minted.to_string());
                (Save::Yes, minted)
            };
            assert_eq!(save, Save::No, "a stored workspace asked for a write");
            id
        };

        assert_eq!(read(), stored, "the stored identity was not returned");
        assert_eq!(read(), stored, "the identity moved between reads");
        assert_eq!(
            std::fs::metadata(preferences_path(&paths))
                .expect("metadata")
                .ino(),
            before,
            "the file was rewritten by a read"
        );
    }

    /// Two mutations of DIFFERENT fields must both survive. Each loads the
    /// whole file, changes one field, and renames a full snapshot back — so
    /// without a lock around the whole cycle the last writer erases the other's
    /// field while both report success.
    #[test]
    fn concurrent_mutations_of_different_fields_do_not_lose_each_other() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.config_dir).expect("config dir");

        // The guarded cycle, addressed at this test's paths rather than the
        // process-wide ones: same lock, same load-mutate-save shape.
        let mutate = |change: &dyn Fn(&mut Preferences)| {
            let _guard = PREFERENCES_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut preferences = load_preferences(&paths).expect("load");
            change(&mut preferences);
            save_preferences(&paths, &preferences).expect("save");
        };

        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..200 {
                    mutate(&|preferences| {
                        preferences.repository = Some("/tmp/a-checkout".to_string());
                    });
                }
            });
            scope.spawn(|| {
                for _ in 0..200 {
                    mutate(&|preferences| {
                        preferences.run_defaults.model = Some("a-model".to_string());
                    });
                }
            });
        });

        let loaded = load_preferences(&paths).expect("load");
        assert_eq!(
            loaded.repository.as_deref(),
            Some("/tmp/a-checkout"),
            "the repository was lost to the other field's writer"
        );
        assert_eq!(
            loaded.run_defaults.model.as_deref(),
            Some("a-model"),
            "the run default was lost to the other field's writer"
        );
    }

    fn init_repo(path: &Path) {
        // Captured, not inherited: a bare `assert!(status.success())` here
        // reported only "assertion failed" when this went red in CI, which cost
        // an hour of digging to find that another module's test had exported
        // GIT_DIR into this process.
        let output = std::process::Command::new("git")
            .current_dir(path)
            .args(["init", "--quiet"])
            .output()
            .expect("git init");
        assert!(
            output.status.success(),
            "git init failed in {}: {}{}",
            path.display(),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout),
        );
    }

    #[test]
    fn a_subdirectory_anchors_to_the_checkout_root() {
        let repo = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path());
        let nested = repo.path().join("src").join("deep");
        std::fs::create_dir_all(&nested).expect("mkdir -p");

        let root = validate_repository(repo.path()).expect("root is a checkout");
        let from_nested = validate_repository(&nested).expect("nested anchors");
        assert_eq!(from_nested.path, root.path);
        // And the operator is told their pick was anchored upward.
        assert!(from_nested.picked.is_some());
        assert!(root.picked.is_none());
    }

    #[test]
    fn a_directory_that_is_not_a_checkout_is_refused() {
        let plain = tempfile::tempdir().expect("tempdir");
        let error = validate_repository(plain.path()).expect_err("must refuse");
        assert!(
            format!("{error:#}").contains("not a git checkout"),
            "unexpected refusal: {error:#}"
        );
    }

    /// The incident: `$HOME` must be refused even if it somehow answers as a
    /// checkout. Asserted through the filesystem gate directly, because making
    /// the real home directory a repository in a test is not acceptable.
    #[test]
    fn the_home_directory_is_never_a_plausible_root() {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        assert!(!plausible_repository_root(Path::new(&home)));
    }

    #[test]
    fn account_roots_are_never_plausible() {
        assert!(!plausible_repository_root(Path::new("/")));
        assert!(!plausible_repository_root(Path::new("/Users")));
    }

    /// A selection is stored and read back through the real preferences file.
    #[test]
    fn a_selection_round_trips_through_the_preferences_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = crate::runtime_paths::resolve().expect("runtime paths");
        let paths = RuntimePaths {
            config_dir: dir.path().to_path_buf(),
            ..paths
        };
        assert!(load_preferences(&paths)
            .expect("missing is empty")
            .repository
            .is_none());
        save_preferences(
            &paths,
            &Preferences {
                repository: Some("/tmp/example".to_owned()),
                run_defaults: StoredRunDefaults::default(),
                workspace: None,
            },
        )
        .expect("save");
        assert_eq!(
            load_preferences(&paths)
                .expect("load")
                .repository
                .as_deref(),
            Some("/tmp/example")
        );
    }

    /// A corrupt preferences file is an error, never silently "nothing chosen".
    #[test]
    fn a_corrupt_preferences_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(PREFERENCES_FILE), b"{ not json").expect("write");
        let paths = crate::runtime_paths::resolve().expect("runtime paths");
        let paths = RuntimePaths {
            config_dir: dir.path().to_path_buf(),
            ..paths
        };
        assert!(load_preferences(&paths).is_err());
    }
}
