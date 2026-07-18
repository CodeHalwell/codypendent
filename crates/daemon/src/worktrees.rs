//! Git worktree manager (STEP 1.8).
//!
//! Every writing run is isolated in a dedicated Git worktree that lives
//! **outside** the repository working tree (a sibling `codypendent-worktrees/`
//! directory), on a per-run branch `codypendent/run-<short-run-id>`. Git remains
//! the authority; the `workspace_leases` table is a durable index over it
//! ([Chapter 04](../../../docs/docs/04-agent-runtime-and-workflows.md),
//! [STEP 1.8](../../../docs/docs/build/11-phase-1-persistent-agent-slice.md)).
//!
//! Three operations make up the contract:
//! - [`WorktreeManager::allocate`] mints a lease + branch + worktree.
//! - [`WorktreeManager::release`] tears one down, but **protects unmerged work**:
//!   if the branch has commits the base does not, or the working tree is dirty,
//!   it exports a patch artifact and retains the directory unless `force` is set.
//! - [`WorktreeManager::reconcile_on_startup`] compares lease rows against
//!   `git worktree list --porcelain` and marks inconsistencies `orphaned`. It
//!   never deletes anything on startup.
//!
//! Every `git` invocation is a direct process spawn with an explicit argument
//! list — never a shell string — so repository paths can never be interpreted as
//! shell syntax.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};
use codypendent_protocol::{ArtifactRef, DataClassification, RunId};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::process::Command;
use uuid::Uuid;

use crate::artifacts::{ArtifactStore, Provenance};

/// How long an allocated lease is considered valid. Leases are advisory records
/// over Git; the TTL exists only so the `expires_at` column is populated and a
/// future reaper can find abandoned rows.
const LEASE_TTL_HOURS: i64 = 24;

/// The write mode a lease was granted under. Phase 1 only allocates `Write`
/// leases; `Read` exists so the enum mirrors the [Chapter 14] `WorkspaceLease`
/// contract and can round-trip a `read` row written by a later phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeaseMode {
    /// The single writable lease for a worktree.
    Write,
    /// A non-exclusive read lease (unused in Phase 1).
    Read,
}

impl LeaseMode {
    fn as_db(self) -> &'static str {
        match self {
            LeaseMode::Write => "write",
            LeaseMode::Read => "read",
        }
    }

    fn from_db(s: &str) -> Self {
        match s {
            "read" => LeaseMode::Read,
            _ => LeaseMode::Write,
        }
    }
}

/// The lifecycle state of a lease row, mirroring the `state` column
/// (`active | released | orphaned`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeaseState {
    /// The worktree is live and owned by its run.
    Active,
    /// The lease has been torn down (directory removed, or retained with work
    /// preserved as a patch artifact).
    Released,
    /// Reconciliation found the row inconsistent with Git; needs manual review.
    Orphaned,
}

impl LeaseState {
    fn as_db(self) -> &'static str {
        match self {
            LeaseState::Active => "active",
            LeaseState::Released => "released",
            LeaseState::Orphaned => "orphaned",
        }
    }

    fn from_db(s: &str) -> Self {
        match s {
            "released" => LeaseState::Released,
            "orphaned" => LeaseState::Orphaned,
            _ => LeaseState::Active,
        }
    }
}

/// A daemon-local mirror of the [Chapter 14] `WorkspaceLease` contract, one per
/// `workspace_leases` row. `id` is a daemon-local UUID (the protocol crate does
/// not define a `WorkspaceLeaseId` newtype yet); `owner_run_id` is the typed
/// [`RunId`] the lease belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceLease {
    pub id: Uuid,
    pub repository_path: PathBuf,
    pub worktree_path: PathBuf,
    pub branch: String,
    pub base_commit: String,
    pub owner_run_id: RunId,
    pub mode: LeaseMode,
    pub state: LeaseState,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub released_at: Option<DateTime<Utc>>,
}

/// What [`WorktreeManager::release`] did with a lease.
#[derive(Debug, Clone)]
pub struct ReleaseOutcome {
    /// The lease that was released (always ends in [`LeaseState::Released`]).
    pub lease_id: Uuid,
    /// `true` when the worktree directory was retained because it held work
    /// that would otherwise be lost (and `force` was not set).
    pub preserved: bool,
    /// `true` when the worktree directory was removed from disk.
    pub worktree_removed: bool,
    /// The exported patch artifact, present whenever unmerged commits or dirty
    /// files were detected (the safety net for "protect unmerged work").
    pub patch: Option<ArtifactRef>,
    /// Number of commits on the branch that the base commit does not contain.
    pub unmerged_commits: usize,
    /// Whether the worktree had uncommitted changes.
    pub dirty: bool,
}

/// The result of [`WorktreeManager::reconcile_on_startup`].
#[derive(Debug, Clone, Default)]
pub struct ReconcileReport {
    /// Lease ids whose worktree directory was missing; marked [`LeaseState::Orphaned`].
    pub orphaned_leases: Vec<Uuid>,
    /// Worktree directories Git still tracks that have no lease row, flagged for
    /// manual cleanup. Never auto-deleted, and never auto-inserted (their owner
    /// run is unknown, and `owner_run_id` is a non-null foreign key).
    pub adopted_orphans: Vec<PathBuf>,
}

/// A structured worktree-management error. Every variant is machine-branchable;
/// raw `sqlx`/`git` failures are wrapped, never surfaced verbatim to callers.
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    /// The computed worktree path would sit inside the repository working tree.
    /// Worktrees must live outside it (STEP 1.8 requirement 1).
    #[error("worktree path {worktree} would be nested inside repository {repository}")]
    NestedWorktree {
        repository: PathBuf,
        worktree: PathBuf,
    },
    /// A second writable lease was requested for a worktree that already has an
    /// active one (STEP 1.8 requirement 4). Distinct from a raw unique-constraint
    /// error so callers can branch on it.
    #[error("worktree {worktree_path} already has an active lease")]
    LeaseConflict { worktree_path: PathBuf },
    /// No lease row exists for the given id.
    #[error("no workspace lease with id {lease_id}")]
    LeaseNotFound { lease_id: Uuid },
    /// A `git` invocation exited non-zero.
    #[error("`{command}` failed: {stderr}")]
    Git { command: String, stderr: String },
    /// A stored lease row could not be decoded (should never happen; the daemon
    /// wrote it).
    #[error("corrupt lease row: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Wraps an [`ArtifactStore`] failure during patch export.
    #[error(transparent)]
    Artifact(anyhow::Error),
}

/// Allocates, releases, and reconciles per-run Git worktrees over the
/// `workspace_leases` table.
///
/// The default layout places worktrees in `<repo>/../codypendent-worktrees/`.
/// [`WorktreeManager::with_base`] overrides the parent directory; it exists so a
/// test can point the base *inside* the repository and prove the nested-path
/// guard rejects it — production code always uses [`WorktreeManager::new`].
#[derive(Debug, Clone, Default)]
pub struct WorktreeManager {
    base_override: Option<PathBuf>,
}

impl WorktreeManager {
    /// A manager using the normative sibling-directory layout.
    pub fn new() -> Self {
        Self {
            base_override: None,
        }
    }

    /// A manager that creates worktrees directly under `base` (as
    /// `base/run-<short-id>`) instead of the sibling layout.
    pub fn with_base(base: PathBuf) -> Self {
        Self {
            base_override: Some(base),
        }
    }

    /// Create the branch `codypendent/run-<short>` at the repository's current
    /// HEAD and a worktree checked out to it, then persist an `active` write
    /// lease. The worktree path must resolve outside the repository working tree
    /// and must not already hold an active lease.
    pub async fn allocate(
        &self,
        pool: &SqlitePool,
        repository: &Path,
        run_id: RunId,
    ) -> Result<WorkspaceLease, WorktreeError> {
        let repo = tokio::fs::canonicalize(repository).await?;
        let short = short_run_id(run_id);
        let branch = format!("codypendent/run-{short}");
        let worktree_path = self.worktree_path_for(&repo, &short)?;

        // Requirement 1: worktrees live outside the repository working tree.
        ensure_outside_repository(&repo, &worktree_path)?;

        // Requirement 4: at most one active lease per worktree path. Pre-check for
        // a clean error before touching Git (the UNIQUE index is the backstop).
        if active_lease_exists(pool, &worktree_path).await? {
            return Err(WorktreeError::LeaseConflict { worktree_path });
        }

        // Record the base commit, then create branch + worktree atomically. Using
        // `add -b <branch> <path> <base>` creates the branch at HEAD (== base) and
        // checks it out into the new worktree in one step, leaving no dangling
        // branch if the add fails.
        let base_commit = run_git(&repo, &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_string();
        if let Some(parent) = worktree_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let add_args: Vec<OsString> = vec![
            "worktree".into(),
            "add".into(),
            "-b".into(),
            branch.clone().into(),
            worktree_path.clone().into_os_string(),
            base_commit.clone().into(),
        ];
        run_git(&repo, &add_args).await?;

        let now = Utc::now();
        let lease = WorkspaceLease {
            id: Uuid::now_v7(),
            repository_path: repo,
            worktree_path: worktree_path.clone(),
            branch,
            base_commit,
            owner_run_id: run_id,
            mode: LeaseMode::Write,
            state: LeaseState::Active,
            created_at: now,
            expires_at: now + Duration::hours(LEASE_TTL_HOURS),
            released_at: None,
        };

        if let Err(e) = insert_lease(pool, &lease).await {
            // Backstop for a lost race on the UNIQUE(worktree_path) index.
            if let WorktreeError::Database(sqlx::Error::Database(db)) = &e {
                if db.is_unique_violation() {
                    return Err(WorktreeError::LeaseConflict { worktree_path });
                }
            }
            return Err(e);
        }

        Ok(lease)
    }

    /// Tear down a lease, protecting work that is not yet in the repository.
    ///
    /// Reconciles against `git worktree list --porcelain`, then checks for
    /// unmerged commits (`git log <base>..<branch>`) and a dirty working tree
    /// (`git status --porcelain`). If either exists and `force` is false, the
    /// combined diff is exported as a patch artifact and the directory is
    /// **retained**; the lease is still marked `released`. Otherwise the worktree
    /// is removed. This is the "worktree cleanup protects unmerged work" exit
    /// criterion.
    pub async fn release(
        &self,
        pool: &SqlitePool,
        artifacts: &ArtifactStore,
        lease_id: Uuid,
        force: bool,
    ) -> Result<ReleaseOutcome, WorktreeError> {
        let lease = fetch_lease(pool, lease_id)
            .await?
            .ok_or(WorktreeError::LeaseNotFound { lease_id })?;
        let repo = &lease.repository_path;
        let worktree = &lease.worktree_path;

        // Reconcile with Git's own view before mutating anything.
        let registered = worktree_is_registered(repo, worktree).await;
        let worktree_present = worktree.exists();

        // Unmerged commits: on the branch but not reachable from the base commit.
        let range = format!("{}..{}", lease.base_commit, lease.branch);
        let unmerged_commits = match run_git(repo, &["log", &range, "--oneline"]).await {
            Ok(out) => out.lines().filter(|l| !l.trim().is_empty()).count(),
            Err(_) => 0,
        };

        // Dirty working tree (tracked modifications, staged changes, untracked).
        let dirty = if worktree_present {
            match run_git(worktree, &["status", "--porcelain"]).await {
                Ok(out) => !out.trim().is_empty(),
                Err(_) => false,
            }
        } else {
            false
        };

        let has_work = unmerged_commits > 0 || dirty;

        if has_work && !force {
            // Protective path: export a patch and keep the directory.
            let patch = self.export_patch(pool, artifacts, &lease).await?;
            mark_released(pool, lease_id).await?;
            return Ok(ReleaseOutcome {
                lease_id,
                preserved: true,
                worktree_removed: false,
                patch: Some(patch),
                unmerged_commits,
                dirty,
            });
        }

        // If we are forcibly discarding real work, still export it first so it is
        // never lost, then remove the worktree. The export must SUCCEED and be
        // NON-EMPTY before anything is deleted: a failed or empty diff for a
        // worktree that provably has work means the safety patch did not capture
        // it (corrupt base commit, git failure), and force-removing anyway would
        // destroy the only copy. In that case the worktree is preserved instead.
        let patch = if has_work {
            let exported = self.export_patch(pool, artifacts, &lease).await?;
            if exported.byte_length == 0 {
                mark_released(pool, lease_id).await?;
                return Ok(ReleaseOutcome {
                    lease_id,
                    preserved: true,
                    worktree_removed: false,
                    patch: None,
                    unmerged_commits,
                    dirty,
                });
            }
            Some(exported)
        } else {
            None
        };

        let mut removed = false;
        if registered {
            let mut args: Vec<OsString> = vec!["worktree".into(), "remove".into()];
            if force {
                args.push("--force".into());
            }
            args.push(worktree.clone().into_os_string());
            run_git(repo, &args).await?;
            removed = true;
        } else if worktree_present {
            tokio::fs::remove_dir_all(worktree).await?;
            removed = true;
        }

        mark_released(pool, lease_id).await?;
        Ok(ReleaseOutcome {
            lease_id,
            preserved: false,
            worktree_removed: removed,
            patch,
            unmerged_commits,
            dirty,
        })
    }

    /// Reconcile lease rows against reality on daemon startup. Active leases whose
    /// worktree directory has vanished are marked `orphaned`; worktrees Git still
    /// tracks with no lease row are reported for manual cleanup. Nothing is ever
    /// deleted here.
    pub async fn reconcile_on_startup(
        &self,
        pool: &SqlitePool,
    ) -> Result<ReconcileReport, WorktreeError> {
        let leases = all_leases(pool).await?;
        let mut report = ReconcileReport::default();

        // Active rows whose directory is gone become orphaned.
        for lease in &leases {
            if lease.state == LeaseState::Active && !lease.worktree_path.exists() {
                mark_orphaned(pool, lease.id).await?;
                report.orphaned_leases.push(lease.id);
            }
        }

        // Adopt tracked worktrees that have no row. Group known repositories and
        // ask Git; a repository_path that is no longer a Git repo is skipped.
        let mut repos: Vec<PathBuf> = leases.iter().map(|l| l.repository_path.clone()).collect();
        repos.sort();
        repos.dedup();
        let known: Vec<PathBuf> = leases
            .iter()
            .map(|l| canonicalize_lenient(&l.worktree_path))
            .collect();

        for repo in repos {
            let Ok(listing) = run_git(&repo, &["worktree", "list", "--porcelain"]).await else {
                continue;
            };
            // Our per-run worktrees are the ones that live under the managed base
            // directory for this repository — identify them by *path*, not branch
            // name, so a detached-HEAD worktree (branch == None) is adopted too
            // instead of being silently skipped.
            let managed_base = self
                .managed_base_for(&repo)
                .map(|b| canonicalize_lenient(&b));
            for record in parse_worktree_list(&listing) {
                let canon = canonicalize_lenient(&record.path);
                let is_ours = managed_base
                    .as_ref()
                    .is_some_and(|base| canon.starts_with(base));
                if !is_ours {
                    continue;
                }
                if !known.contains(&canon) {
                    report.adopted_orphans.push(record.path);
                }
            }
        }

        Ok(report)
    }

    /// The base directory this manager places `repo`'s worktrees under, matching
    /// [`worktree_path_for`](Self::worktree_path_for)'s layout: the override base
    /// if set, else `<repo>/../codypendent-worktrees/<repo-name>`. `None` when the
    /// repository path has no parent or final component.
    fn managed_base_for(&self, repo: &Path) -> Option<PathBuf> {
        if let Some(base) = &self.base_override {
            return Some(base.clone());
        }
        let parent = repo.parent()?;
        let repo_name = repo.file_name()?;
        Some(parent.join("codypendent-worktrees").join(repo_name))
    }

    /// Compute the worktree path for a run. Default layout is
    /// `<repo>/../codypendent-worktrees/<repo-name>/run-<short>`; an override base
    /// yields `<base>/run-<short>`.
    fn worktree_path_for(&self, repo: &Path, short: &str) -> Result<PathBuf, WorktreeError> {
        let leaf = format!("run-{short}");
        if let Some(base) = &self.base_override {
            return Ok(base.join(leaf));
        }
        let parent = repo
            .parent()
            .ok_or_else(|| WorktreeError::Corrupt("repository path has no parent".into()))?;
        let repo_name = repo.file_name().ok_or_else(|| {
            WorktreeError::Corrupt("repository path has no final component".into())
        })?;
        Ok(parent
            .join("codypendent-worktrees")
            .join(repo_name)
            .join(leaf))
    }

    /// Export the diff from the lease's base commit to the current worktree state
    /// (committed *and* uncommitted tracked changes) as a `text/x-diff` artifact.
    async fn export_patch(
        &self,
        pool: &SqlitePool,
        artifacts: &ArtifactStore,
        lease: &WorkspaceLease,
    ) -> Result<ArtifactRef, WorktreeError> {
        // `git diff <base>` in the worktree spans base -> working tree, capturing
        // both merged-into-branch commits and uncommitted edits in one patch.
        // `--binary` so binary file content survives (a plain diff records only
        // "Binary files differ" — useless for restoration). A diff FAILURE
        // propagates: swallowing it would store an empty "safety patch" and let
        // a force-release destroy the only copy of the work.
        let diff = if lease.worktree_path.exists() {
            // `git diff` omits *untracked* files, but a force-release that is
            // about to delete the worktree would then lose them silently. Mark
            // them intent-to-add first so they appear in the diff as additions
            // (the worktree is being torn down, so mutating its index is fine).
            let _ = run_git(&lease.worktree_path, &["add", "-A", "--intent-to-add"]).await;
            run_git(
                &lease.worktree_path,
                &["diff", "--binary", &lease.base_commit],
            )
            .await?
        } else {
            let range = format!("{}..{}", lease.base_commit, lease.branch);
            run_git(&lease.repository_path, &["diff", "--binary", &range]).await?
        };

        artifacts
            .put(
                pool,
                "text/x-diff",
                DataClassification::Internal,
                Provenance::system(format!("worktree-release:{}", lease.id)),
                diff.as_bytes(),
            )
            .await
            .map_err(WorktreeError::Artifact)
    }
}

/// A collision-resistant short id for a run: the **last** 12 hex characters of
/// the run id's UUIDv7. The high bits of a v7 UUID are a shared millisecond
/// clock — runs minted within ~65s share their leading hex digits — so the tail
/// (the random component) is used instead. The `codypendent/run-` prefix stays.
fn short_run_id(run_id: RunId) -> String {
    let simple = run_id.0.as_simple().to_string();
    simple[simple.len() - 12..].to_string()
}

/// Reject a worktree path that resolves inside the repository working tree.
fn ensure_outside_repository(repo: &Path, worktree: &Path) -> Result<(), WorktreeError> {
    let resolved = canonicalize_lenient(worktree);
    if resolved.starts_with(repo) {
        return Err(WorktreeError::NestedWorktree {
            repository: repo.to_path_buf(),
            worktree: worktree.to_path_buf(),
        });
    }
    Ok(())
}

/// Spawn `git` with an explicit argument vector (never a shell string) in `dir`,
/// returning stdout on success or a [`WorktreeError::Git`] on a non-zero exit.
async fn run_git<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<String, WorktreeError> {
    let mut command = Command::new("git");
    command.current_dir(dir);
    for arg in args {
        command.arg(arg);
    }
    let output = command.output().await?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let printable: Vec<String> = args
            .iter()
            .map(|a| a.as_ref().to_string_lossy().into_owned())
            .collect();
        Err(WorktreeError::Git {
            command: format!("git {}", printable.join(" ")),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// True if `worktree` appears in `git worktree list --porcelain` run from `repo`.
async fn worktree_is_registered(repo: &Path, worktree: &Path) -> bool {
    let Ok(listing) = run_git(repo, &["worktree", "list", "--porcelain"]).await else {
        return false;
    };
    let target = canonicalize_lenient(worktree);
    parse_worktree_list(&listing)
        .iter()
        .any(|r| canonicalize_lenient(&r.path) == target)
}

/// One record parsed from `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeRecord {
    path: PathBuf,
    branch: Option<String>,
}

/// Parse the porcelain worktree listing into typed records. Records are
/// separated by blank lines; each begins with a `worktree <path>` line and may
/// carry a `branch refs/heads/<name>` line (absent for detached or bare entries).
fn parse_worktree_list(output: &str) -> Vec<WorktreeRecord> {
    let mut records = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;

    let mut flush = |path: &mut Option<PathBuf>, branch: &mut Option<String>| {
        if let Some(p) = path.take() {
            records.push(WorktreeRecord {
                path: p,
                branch: branch.take(),
            });
        } else {
            *branch = None;
        }
    };

    for line in output.lines() {
        if line.is_empty() {
            flush(&mut path, &mut branch);
        } else if let Some(rest) = line.strip_prefix("worktree ") {
            // A new record starts; flush any in-progress one first.
            flush(&mut path, &mut branch);
            path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = Some(rest.to_string());
        }
    }
    flush(&mut path, &mut branch);
    records
}

/// Canonicalize `path`, or if it does not exist yet, canonicalize the nearest
/// existing ancestor (resolving symlinks and `..` there) and re-append the
/// remainder, collapsing `.`/`..` lexically.
fn canonicalize_lenient(path: &Path) -> PathBuf {
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

// --- Persistence -----------------------------------------------------------

type LeaseRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
);

const LEASE_COLUMNS: &str = "id, repository_path, worktree_path, branch, base_commit, \
     owner_run_id, mode, state, created_at, expires_at, released_at";

async fn active_lease_exists(pool: &SqlitePool, worktree: &Path) -> Result<bool, WorktreeError> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM workspace_leases WHERE worktree_path = ? AND state = 'active'",
    )
    .bind(worktree.to_string_lossy().as_ref())
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

async fn insert_lease(pool: &SqlitePool, lease: &WorkspaceLease) -> Result<(), WorktreeError> {
    sqlx::query(&format!(
        "INSERT INTO workspace_leases ({LEASE_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(lease.id.to_string())
    .bind(lease.repository_path.to_string_lossy().as_ref())
    .bind(lease.worktree_path.to_string_lossy().as_ref())
    .bind(&lease.branch)
    .bind(&lease.base_commit)
    .bind(lease.owner_run_id.to_string())
    .bind(lease.mode.as_db())
    .bind(lease.state.as_db())
    .bind(lease.created_at.to_rfc3339())
    .bind(lease.expires_at.to_rfc3339())
    .bind(lease.released_at.map(|t| t.to_rfc3339()))
    .execute(pool)
    .await?;
    Ok(())
}

async fn fetch_lease(
    pool: &SqlitePool,
    lease_id: Uuid,
) -> Result<Option<WorkspaceLease>, WorktreeError> {
    let row: Option<LeaseRow> = sqlx::query_as(&format!(
        "SELECT {LEASE_COLUMNS} FROM workspace_leases WHERE id = ?"
    ))
    .bind(lease_id.to_string())
    .fetch_optional(pool)
    .await?;
    row.map(lease_from_row).transpose()
}

async fn all_leases(pool: &SqlitePool) -> Result<Vec<WorkspaceLease>, WorktreeError> {
    let rows: Vec<LeaseRow> =
        sqlx::query_as(&format!("SELECT {LEASE_COLUMNS} FROM workspace_leases"))
            .fetch_all(pool)
            .await?;
    rows.into_iter().map(lease_from_row).collect()
}

async fn mark_released(pool: &SqlitePool, lease_id: Uuid) -> Result<(), WorktreeError> {
    sqlx::query("UPDATE workspace_leases SET state = 'released', released_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(lease_id.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

async fn mark_orphaned(pool: &SqlitePool, lease_id: Uuid) -> Result<(), WorktreeError> {
    sqlx::query("UPDATE workspace_leases SET state = 'orphaned' WHERE id = ?")
        .bind(lease_id.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

fn lease_from_row(row: LeaseRow) -> Result<WorkspaceLease, WorktreeError> {
    let (
        id,
        repository_path,
        worktree_path,
        branch,
        base_commit,
        owner_run_id,
        mode,
        state,
        created_at,
        expires_at,
        released_at,
    ) = row;
    Ok(WorkspaceLease {
        id: Uuid::from_str(&id).map_err(|e| WorktreeError::Corrupt(format!("id: {e}")))?,
        repository_path: PathBuf::from(repository_path),
        worktree_path: PathBuf::from(worktree_path),
        branch,
        base_commit,
        owner_run_id: RunId::from_str(&owner_run_id)
            .map_err(|e| WorktreeError::Corrupt(format!("owner_run_id: {e}")))?,
        mode: LeaseMode::from_db(&mode),
        state: LeaseState::from_db(&state),
        created_at: parse_ts(&created_at, "created_at")?,
        expires_at: parse_ts(&expires_at, "expires_at")?,
        released_at: released_at
            .map(|t| parse_ts(&t, "released_at"))
            .transpose()?,
    })
}

fn parse_ts(s: &str, field: &str) -> Result<DateTime<Utc>, WorktreeError> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| WorktreeError::Corrupt(format!("{field}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn test_pool(dir: &Path) -> SqlitePool {
        crate::db::open_database(&dir.join("test.db"))
            .await
            .expect("open database")
    }

    /// Insert a session + run for `run_id` so a lease's `owner_run_id` foreign
    /// key resolves.
    async fn insert_run(pool: &SqlitePool, run_id: RunId) {
        let session_id = codypendent_protocol::SessionId::new();
        let now = Utc::now().to_rfc3339();
        sqlx::query("INSERT INTO sessions (id, title, created_at, updated_at) VALUES (?, ?, ?, ?)")
            .bind(session_id.to_string())
            .bind("worktree-test")
            .bind(&now)
            .bind(&now)
            .execute(pool)
            .await
            .expect("insert session");

        sqlx::query(
            "INSERT INTO runs (id, session_id, objective, state, mode, model_policy, budget_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(run_id.to_string())
        .bind(session_id.to_string())
        .bind("diagnose")
        .bind("Running")
        .bind("Build")
        .bind("hosted-default")
        .bind("{}")
        .execute(pool)
        .await
        .expect("insert run");
    }

    /// Insert a session + run with a fresh id, returning it.
    async fn seed_run(pool: &SqlitePool) -> RunId {
        let run_id = RunId::new();
        insert_run(pool, run_id).await;
        run_id
    }

    /// Run `git` synchronously in a test, asserting success.
    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Initialise a repo *inside* `parent` (so its sibling worktree tree is also
    /// under `parent` and cleaned up with the tempdir) and make an initial commit.
    fn init_repo(parent: &Path) -> PathBuf {
        let repo = parent.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "test@codypendent.dev"]);
        git(&repo, &["config", "user.name", "Codypendent Test"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "initial"]);
        repo
    }

    async fn lease_state(pool: &SqlitePool, id: Uuid) -> LeaseState {
        let (state,): (String,) = sqlx::query_as("SELECT state FROM workspace_leases WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(pool)
            .await
            .expect("fetch state");
        LeaseState::from_db(&state)
    }

    #[tokio::test]
    async fn allocate_creates_branch_and_outside_worktree() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let repo = init_repo(dir.path());
        let run_id = seed_run(&pool).await;

        let mgr = WorktreeManager::new();
        let lease = mgr.allocate(&pool, &repo, run_id).await.unwrap();

        assert_eq!(lease.state, LeaseState::Active);
        assert_eq!(lease.mode, LeaseMode::Write);
        assert!(lease.branch.starts_with("codypendent/run-"));
        assert!(lease.worktree_path.exists(), "worktree directory created");
        assert!(
            !lease.worktree_path.starts_with(&lease.repository_path),
            "worktree must live outside the repository tree"
        );
        assert!(!lease.base_commit.is_empty());
    }

    #[tokio::test]
    async fn unmerged_commit_is_protected_on_release() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let repo = init_repo(dir.path());
        let run_id = seed_run(&pool).await;
        let store = ArtifactStore::new(dir.path().join("artifacts"));

        let mgr = WorktreeManager::new();
        let lease = mgr.allocate(&pool, &repo, run_id).await.unwrap();

        // Commit new work on the run branch, inside the worktree.
        let wt = &lease.worktree_path;
        std::fs::write(wt.join("feature.txt"), "new work\n").unwrap();
        git(wt, &["add", "."]);
        git(wt, &["commit", "-q", "-m", "unmerged feature"]);

        let outcome = mgr.release(&pool, &store, lease.id, false).await.unwrap();

        assert!(outcome.unmerged_commits >= 1);
        assert!(outcome.preserved, "unmerged work must retain the directory");
        assert!(!outcome.worktree_removed);
        assert!(outcome.patch.is_some(), "a patch artifact must be exported");
        assert_eq!(lease_state(&pool, lease.id).await, LeaseState::Released);
        assert!(wt.exists(), "worktree directory must still exist");

        // The patch artifact row really exists in the store.
        let patch = outcome.patch.unwrap();
        assert!(store.verify(&pool, patch.id).await.unwrap());
    }

    #[tokio::test]
    async fn dirty_file_is_preserved_on_release() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let repo = init_repo(dir.path());
        let run_id = seed_run(&pool).await;
        let store = ArtifactStore::new(dir.path().join("artifacts"));

        let mgr = WorktreeManager::new();
        let lease = mgr.allocate(&pool, &repo, run_id).await.unwrap();

        // Leave an uncommitted change to a tracked file in the worktree.
        let wt = &lease.worktree_path;
        std::fs::write(wt.join("README.md"), "hello\nlocal edit\n").unwrap();

        let outcome = mgr.release(&pool, &store, lease.id, false).await.unwrap();

        assert_eq!(outcome.unmerged_commits, 0);
        assert!(outcome.dirty, "uncommitted change must be detected");
        assert!(outcome.preserved);
        assert!(!outcome.worktree_removed);
        assert!(outcome.patch.is_some());
        assert_eq!(lease_state(&pool, lease.id).await, LeaseState::Released);
        assert!(wt.exists(), "dirty worktree directory must be preserved");
    }

    #[tokio::test]
    async fn clean_release_removes_worktree() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let repo = init_repo(dir.path());
        let run_id = seed_run(&pool).await;
        let store = ArtifactStore::new(dir.path().join("artifacts"));

        let mgr = WorktreeManager::new();
        let lease = mgr.allocate(&pool, &repo, run_id).await.unwrap();
        let wt = lease.worktree_path.clone();

        let outcome = mgr.release(&pool, &store, lease.id, false).await.unwrap();

        assert!(!outcome.preserved);
        assert!(outcome.worktree_removed);
        assert!(outcome.patch.is_none());
        assert_eq!(lease_state(&pool, lease.id).await, LeaseState::Released);
        assert!(!wt.exists(), "clean worktree directory must be removed");
    }

    #[tokio::test]
    async fn force_release_removes_worktree_with_unmerged_work() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let repo = init_repo(dir.path());
        let run_id = seed_run(&pool).await;
        let store = ArtifactStore::new(dir.path().join("artifacts"));

        let mgr = WorktreeManager::new();
        let lease = mgr.allocate(&pool, &repo, run_id).await.unwrap();
        let wt = lease.worktree_path.clone();
        std::fs::write(wt.join("scratch.txt"), "throwaway\n").unwrap();

        let outcome = mgr.release(&pool, &store, lease.id, true).await.unwrap();

        assert!(
            outcome.worktree_removed,
            "force removes even dirty worktrees"
        );
        assert!(!outcome.preserved);
        assert_eq!(lease_state(&pool, lease.id).await, LeaseState::Released);
        assert!(!wt.exists());
    }

    #[tokio::test]
    async fn force_release_preserves_untracked_file_in_patch() {
        use tokio::io::AsyncReadExt;

        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let repo = init_repo(dir.path());
        let run_id = seed_run(&pool).await;
        let store = ArtifactStore::new(dir.path().join("artifacts"));

        let mgr = WorktreeManager::new();
        let lease = mgr.allocate(&pool, &repo, run_id).await.unwrap();
        let wt = lease.worktree_path.clone();

        // The ONLY local change is a brand-new *untracked* file. `git diff <base>`
        // alone would omit it, so a force-release must intent-to-add it first.
        std::fs::write(wt.join("untracked.txt"), "precious untracked work\n").unwrap();

        let outcome = mgr.release(&pool, &store, lease.id, true).await.unwrap();

        assert!(outcome.dirty, "an untracked file makes the worktree dirty");
        assert!(outcome.worktree_removed, "force removes the worktree");
        let patch = outcome
            .patch
            .expect("force-discarding real work exports a safety patch");
        assert!(store.verify(&pool, patch.id).await.unwrap());

        // The exported patch actually contains the untracked file and its content.
        let mut file = store.open(&pool, patch.id).await.unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).await.unwrap();
        let patch_text = String::from_utf8_lossy(&bytes);
        assert!(
            patch_text.contains("untracked.txt"),
            "patch must name the untracked file, got:\n{patch_text}"
        );
        assert!(
            patch_text.contains("precious untracked work"),
            "patch must carry the untracked content, got:\n{patch_text}"
        );
    }

    #[tokio::test]
    async fn stale_record_is_reconciled_to_orphaned() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let run_id = seed_run(&pool).await;

        // A lease row pointing at a directory that does not exist.
        let missing = dir.path().join("gone").join("run-deadbeef");
        let lease = WorkspaceLease {
            id: Uuid::now_v7(),
            repository_path: dir.path().join("not-a-repo"),
            worktree_path: missing.clone(),
            branch: "codypendent/run-deadbeef".to_string(),
            base_commit: "0".repeat(40),
            owner_run_id: run_id,
            mode: LeaseMode::Write,
            state: LeaseState::Active,
            created_at: Utc::now(),
            expires_at: Utc::now(),
            released_at: None,
        };
        insert_lease(&pool, &lease).await.unwrap();

        let mgr = WorktreeManager::new();
        let report = mgr.reconcile_on_startup(&pool).await.unwrap();

        assert!(report.orphaned_leases.contains(&lease.id));
        assert_eq!(lease_state(&pool, lease.id).await, LeaseState::Orphaned);
        // Nothing was created or deleted.
        assert!(!missing.exists());
    }

    #[tokio::test]
    async fn simultaneous_allocations_get_distinct_worktrees() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let repo = init_repo(dir.path());

        // Two genuinely distinct runs. The short id is the last 12 hex chars of
        // the run id — the v7 random tail — so distinct runs get distinct
        // worktrees even when their high (millisecond-clock) bits coincide. These
        // two share nothing relevant and differ in that tail (…0001 vs …0002).
        let run_a = RunId(Uuid::from_u128(0xaaaa_aaaa_0000_7000_8000_0000_0000_0001));
        let run_b = RunId(Uuid::from_u128(0xbbbb_bbbb_0000_7000_8000_0000_0000_0002));
        insert_run(&pool, run_a).await;
        insert_run(&pool, run_b).await;

        let mgr = WorktreeManager::new();
        let a = mgr.allocate(&pool, &repo, run_a).await.unwrap();
        let b = mgr.allocate(&pool, &repo, run_b).await.unwrap();

        assert_ne!(a.worktree_path, b.worktree_path);
        assert_ne!(a.branch, b.branch);
        assert_eq!(lease_state(&pool, a.id).await, LeaseState::Active);
        assert_eq!(lease_state(&pool, b.id).await, LeaseState::Active);
        assert!(a.worktree_path.exists() && b.worktree_path.exists());
    }

    #[tokio::test]
    async fn second_active_lease_for_same_worktree_conflicts() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let repo = init_repo(dir.path());
        let run_id = seed_run(&pool).await;

        let mgr = WorktreeManager::new();
        mgr.allocate(&pool, &repo, run_id).await.unwrap();
        // The same run id maps to the same worktree path -> conflict.
        let err = mgr.allocate(&pool, &repo, run_id).await.unwrap_err();
        assert!(
            matches!(err, WorktreeError::LeaseConflict { .. }),
            "expected LeaseConflict, got {err:?}"
        );
    }

    #[tokio::test]
    async fn nested_worktree_path_is_rejected() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let repo = init_repo(dir.path());
        let run_id = seed_run(&pool).await;

        // Force the base *inside* the repository working tree.
        let mgr = WorktreeManager::with_base(repo.join("inside-worktrees"));
        let err = mgr.allocate(&pool, &repo, run_id).await.unwrap_err();
        assert!(
            matches!(err, WorktreeError::NestedWorktree { .. }),
            "expected NestedWorktree, got {err:?}"
        );

        // The rejection happened before any row was written.
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM workspace_leases")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn parse_worktree_list_extracts_records() {
        let sample = "worktree /repo\nHEAD abc\nbranch refs/heads/main\n\n\
                      worktree /wt/run-1234\nHEAD def\nbranch refs/heads/codypendent/run-1234\n\n\
                      worktree /wt/detached\nHEAD 999\ndetached\n";
        let records = parse_worktree_list(sample);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].path, PathBuf::from("/repo"));
        assert_eq!(records[0].branch.as_deref(), Some("refs/heads/main"));
        assert_eq!(
            records[1].branch.as_deref(),
            Some("refs/heads/codypendent/run-1234")
        );
        assert_eq!(records[2].path, PathBuf::from("/wt/detached"));
        assert_eq!(records[2].branch, None);
    }
}
