//! The bounded code-graph warm-up scan, shared by startup and per-run launch.
//!
//! At startup `main` scans the daemon's own working directory so the repository
//! map is non-empty from the first run; when a run arrives for a *different*
//! checkout (a per-user daemon can serve several — issue #6 item 1), the executor
//! scans that repository the first time it sees it. Both paths want the same
//! bounded, failure-tolerant walk, so it lives here rather than in `main`.

use std::path::{Path, PathBuf};

use codypendent_knowledge::{codegraph, GitRevision};
use codypendent_protocol::RepositoryId;
use sqlx::SqlitePool;
use tracing::{info, warn};

/// The upper bound on files folded into the code graph in one scan. The scan is
/// capped so a very large tree never delays the socket opening (startup) or a
/// run's first note — but the cap must comfortably cover a real workspace: the
/// `code_nodes` table is cleared and rebuilt from this scan on every boot, so a
/// cap smaller than the repository silently truncates the *authoritative* graph
/// (and, with an unsorted walk, truncates it differently on every boot).
pub const SCAN_FILE_CAP: usize = 2000;

/// Fold up to [`SCAN_FILE_CAP`] of `root`'s `*.rs` files into the code graph for
/// `repository`, so the repository map is populated. Best-effort: a per-file
/// parse/read failure is logged and skipped, never propagated — a warm-up must
/// not block or fail its caller.
///
/// The repository's prior graph is cleared first so symbols removed since the
/// last scan (files deleted outright, which a per-file reparse never revisits)
/// do not linger. The code graph is derived and regenerable, so wiping and
/// rebuilding is safe.
pub async fn scan_repository(pool: &SqlitePool, repository: RepositoryId, root: &Path) {
    let revision = head_revision(root);

    if let Err(error) = codegraph::clear_repository(pool, repository).await {
        warn!(%error, "could not clear the prior code graph before re-scan");
    }

    // The walk is blocking std::fs work — off the async runtime so a large tree
    // does not stall this worker's other tasks.
    let walk_root = root.to_path_buf();
    let files =
        tokio::task::spawn_blocking(move || collect_rust_sources(&walk_root, SCAN_FILE_CAP))
            .await
            .unwrap_or_default();
    let mut scanned = 0usize;
    let mut nodes = 0usize;
    for (relative, source) in files {
        match codegraph::upsert_file_graph(pool, repository, &revision, &relative, &source).await {
            Ok(delta) => {
                scanned += 1;
                nodes += delta.nodes.len();
            }
            Err(error) => {
                warn!(path = %relative, %error, "skipped a file that would not fold into the code graph");
            }
        }
    }
    info!(
        repository = %repository,
        revision = %revision.0,
        files = scanned,
        nodes,
        "code-graph scan complete"
    );
}

/// The working tree's `HEAD` commit as a [`GitRevision`], or the `"workdir"`
/// placeholder when Git is unavailable or `root` is not a repository. Shelling
/// out keeps this free of a Git library dependency.
fn head_revision(root: &Path) -> GitRevision {
    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    GitRevision(head.unwrap_or_else(|| "workdir".to_string()))
}

/// Collect up to `cap` `(repo-relative-path, source)` pairs for the `*.rs` files
/// under `root`, skipping `target/` and hidden (dot-prefixed) directories. A
/// plain iterative walk (no `walkdir` dependency); unreadable entries are
/// skipped. The traversal is **sorted** (per-directory, names ascending) so the
/// cap — if it ever bites — truncates the same files on every boot instead of
/// rebuilding a different graph per `read_dir` ordering.
fn collect_rust_sources(root: &Path, cap: usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= cap {
            break;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        let mut subdirs = Vec::new();
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Skip hidden dirs/files and the build output tree.
            if name.starts_with('.') || name == "target" {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                subdirs.push(path);
            } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                if out.len() >= cap {
                    break;
                }
                let Ok(source) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                out.push((relative, source));
            }
        }
        // LIFO stack: push in reverse so subdirectories pop in ascending order.
        for subdir in subdirs.into_iter().rev() {
            stack.push(subdir);
        }
    }
    out
}

/// Canonicalize `root` (falling back to the path as-given) and derive the stable
/// [`RepositoryId`] the knowledge fabric attributes work under. Kept here so the
/// startup scan and the per-run executor derive identity identically.
#[must_use]
pub fn repository_id_for(root: &Path) -> RepositoryId {
    let canonical = root.canonicalize().unwrap_or_else(|_| PathBuf::from(root));
    codypendent_knowledge::stable_repository_id(&canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_id_is_stable_per_root_and_distinct_across_roots() {
        // The per-run identity (issue #6 item 1) must be deterministic for one
        // checkout — so a run resolves to the same repository across launches —
        // and distinct for different checkouts served by one daemon.
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        assert_eq!(
            repository_id_for(a.path()),
            repository_id_for(a.path()),
            "same root → same repository id"
        );
        assert_ne!(
            repository_id_for(a.path()),
            repository_id_for(b.path()),
            "different roots → different repository ids"
        );
    }
}
