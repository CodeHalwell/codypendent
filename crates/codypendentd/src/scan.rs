//! The bounded code-graph warm-up scan, shared by startup and per-run launch.
//!
//! Session attach and run launch warm a checkout in the background. Both paths
//! want the same bounded, failure-tolerant walk, so it lives here rather than in
//! the server or executor.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use codypendent_knowledge::codegraph::{Language, ScanSummary};
use codypendent_knowledge::{codegraph, GitRevision};
use codypendent_protocol::RepositoryId;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::sync::{mpsc, OwnedMutexGuard};
use tracing::{debug, info, warn};

/// The upper bound on files folded into the code graph in one scan. The scan is
/// capped so a very large tree never delays the socket opening (startup) or a
/// run's first note — but the cap must comfortably cover a real workspace: the
/// `code_nodes` table is rebuilt from this scan on every boot, so a cap smaller
/// than the repository truncates the *authoritative* graph (and, with an unsorted
/// walk, truncates it differently on every boot).
///
/// Only files the ignore rules ACCEPT are counted against it — see
/// [`collect_source_paths`], where an excluded tree is pruned before anything it
/// holds can spend the budget. And hitting it now suppresses the retire pass
/// ([`codegraph::ScanCoverage`]) rather than deleting everything the walk did not
/// get to.
pub const SCAN_FILE_CAP: usize = 2000;

/// The upper bound on SYMBOLS one scan folds into the graph.
///
/// [`SCAN_FILE_CAP`] counts the wrong thing on its own. A file's contribution to
/// the graph is unbounded: measured on a real user database, one vendored
/// `windows-sys` Win32 bindings `mod.rs` folded to **10,218 nodes**, and the next
/// two to 4,959 and 4,947. Two thousand files of that shape are 386,572 nodes —
/// 76% of a 510,904-node graph — from a single tree, all of it inside the file
/// budget, and every run then spent 30-60 seconds in `Preparing` before the model
/// was called. A file budget cannot see that coming; only a node budget can.
///
/// Sized from that same database, by subtraction. The graph totalled 510,904
/// nodes and one junk repository accounted for 386,572 of them, which leaves
/// **124,332 nodes across every real checkout the user had** — a figure the
/// daemon was serving without complaint. So a per-repository budget of 120,000 is
/// roughly the whole of that user's legitimate graph granted to a *single*
/// repository: generous enough that no real checkout should ever meet it, and
/// four times below the graph that produced the 30-60 second stall.
///
/// That is arithmetic on the incident's numbers, not a measurement of this
/// workspace — but this workspace does fit inside it with room to spare: 811
/// files folded, no cap reached, measured by
/// `the_node_budget_admits_this_workspace_whole`, which fails if either budget
/// ever bites on real source.
pub const SCAN_NODE_BUDGET: usize = 120_000;

/// The upper bound on symbols ONE file may contribute.
///
/// Separate from [`SCAN_NODE_BUDGET`] because the two failures are different. The
/// budget is "this repository is bigger than the graph can serve"; this is "this
/// *file* is not hand-written source". A single file worth 2,000 symbols is
/// generated — FFI bindings, a protobuf module, a minified bundle a grammar still
/// parses — and folding it buys the model nothing while costing it a fiftieth of
/// the whole graph. Skipping it is strictly better than letting it push real
/// source out of the budget, which is exactly what happened.
///
/// Sized to sit in the gap between the two populations, both measured:
///
/// | file | nodes | kind |
/// |---|---|---|
/// | vendored `windows-sys` `mod.rs` | 10,218 | generated |
/// | (next two in the same tree) | 4,959 / 4,947 | generated |
/// | **this cap** | **3,000** | |
/// | `crates/runtime/src/agent.rs` | 1,123 | hand-written, the largest here |
/// | `crates/tui/src/reduce.rs` | 942 | hand-written |
/// | `crates/cli/src/tui.rs` | 894 | hand-written |
///
/// The hand-written figures are this workspace's own, printed by
/// `the_node_budget_admits_this_workspace_whole` (811 files folded). So the cap
/// is 2.7x above the largest real file anyone here has written and 1.6x below the
/// smallest of the generated files that caused the incident — and that test fails
/// if the margin ever closes from the source side.
pub const SCAN_FILE_NODE_CAP: usize = 3_000;

/// Serialize every mutation of one repository's code graph.
///
/// Two independent paths trigger a warm-up for the same checkout — the server's
/// `CreateSession` hook and the executor's `spawn_run` — and `codypendent run`
/// issues both back to back. Before this lock they both observed "not folded"
/// and both ran a full [`codegraph::rebuild_repository`] concurrently, which
/// produced `database is locked` (so the revision guard was never recorded and
/// the repository re-scanned forever) and let a run read the repository map
/// *between* another scanner's writes — a torn graph handed to the model
/// (2026-08-13 review, F6). The live watcher
/// adds a third writer, so the lock is the graph's single writer gate: hold it
/// across a full scan AND across an incremental batch.
///
/// A `tokio` mutex, not a `std` one: it is deliberately held across the awaits
/// of the scan. The registry it lives in is keyed by repository, so two
/// different checkouts served by one daemon still scan in parallel.
pub async fn lock_repository(repository: RepositoryId) -> OwnedMutexGuard<()> {
    static LOCKS: OnceLock<Mutex<HashMap<RepositoryId, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let lock = {
        // The `std` mutex guards only the map lookup and is dropped before the
        // await below — never held across one.
        let mut registry = LOCKS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("code-graph lock registry");
        Arc::clone(registry.entry(repository).or_default())
    };
    lock.lock_owned().await
}

/// Fold up to [`SCAN_FILE_CAP`] of `root`'s source files into the code graph for
/// `repository`, so the repository map is populated. Which files those are is
/// [`codegraph::language_for`]'s decision, never this module's. Best-effort: a
/// per-file parse/read failure is logged and skipped, never propagated — a
/// warm-up must not block or fail its caller.
///
/// Every file is refolded in place by [`codegraph::rebuild_repository`], which
/// then retires exactly the paths this walk no longer saw — the one job a
/// per-file reparse can never do, because nothing reparses a file that is gone.
/// It deliberately does **not** wipe the repository first: that discarded every
/// agent-asserted edge on every build and left the graph empty for the length of
/// the scan, in full view of readers that take no lock.
///
/// A walk stopped by [`SCAN_FILE_CAP`] hands the fold
/// [`codegraph::ScanCoverage::Truncated`], which suppresses that retire pass
/// entirely: an unfinished walk has not looked at the paths it did not reach, so
/// it cannot testify that they are gone. A fold stopped by [`SCAN_NODE_BUDGET`],
/// or shortened by [`SCAN_FILE_NODE_CAP`], is truncated for exactly the same
/// reason and says so the same way — see [`apply_node_budget`].
///
/// The root is resolved by [`resolve_scan_root`], which refuses a non-repository,
/// the home directory and a filesystem root alike. It fails closed: an ambiguous
/// root is refused, never walked.
///
/// Returns a [`ScanSummary`] — files seen, folded per language, skipped as
/// unsupported, and whether the cap truncated the walk. It used to return `()`,
/// which meant a repository in a language the graph could not parse walked
/// thousands of files, folded none, reported success, and left an empty graph
/// with nothing anywhere saying why.
///
/// **The caller must hold [`lock_repository`] for `repository`.** This function
/// does not take it itself: the executor's warm-up re-checks its revision guard
/// under the same guard, and a lock taken here could not cover that check.
pub async fn scan_repository(
    pool: &SqlitePool,
    repository: RepositoryId,
    root: &Path,
) -> anyhow::Result<ScanSummary> {
    let root = match resolve_scan_root(root) {
        Ok(root) => root,
        Err(refusal) => anyhow::bail!("{refusal}"),
    };
    // Stamp what this scan actually READS, which is the working tree — not
    // whatever `HEAD` happens to name. A full rescan of a dirty checkout used to
    // fold uncommitted bytes and label them with the bare commit, so `graph
    // status` then compared a dirty tree against a graph carrying no `+workdir`
    // stamp and called the freshly built graph stale (2026-08-13 review,
    // codegraph F6). The incremental watcher already stamps this way; this is the
    // same question, so it is the same function.
    let revision = working_tree_revision(&root);

    // The walk is blocking std::fs work — off the async runtime so a large tree
    // does not stall this worker's other tasks.
    let walk_root = root;
    let (files, mut summary) =
        tokio::task::spawn_blocking(move || collect_sources(&walk_root, SCAN_FILE_CAP))
            .await
            .map_err(|error| anyhow::anyhow!("code-graph walker failed: {error}"))??;
    // Weigh every file before folding any of it. This IS the preflight parse the
    // fold used to do purely to validate — the counts were computed and thrown
    // away — so the budget costs nothing beyond what was already spent.
    let files = apply_node_budget(
        repository,
        files,
        &mut summary,
        SCAN_FILE_NODE_CAP,
        SCAN_NODE_BUDGET,
    )?;
    // The fold begins only after the entire filesystem walk and parse preflight
    // succeeded, so one malformed file cannot leave the graph half-rebuilt. Any
    // later database failure is returned so the caller removes its in-process
    // success marker and retries.
    let rebuild = codegraph::rebuild_repository(
        pool,
        repository,
        &revision,
        files
            .iter()
            .map(|(relative, source, _)| (relative.as_str(), source.as_str())),
        if summary.truncated_by_cap {
            codegraph::ScanCoverage::Truncated
        } else {
            codegraph::ScanCoverage::Complete
        },
    )
    .await?;
    summary.revision = revision.0.clone();
    summary.carried_edges = rebuild.edges;
    summary.retired = rebuild.retired;
    for ((_, _, language), folded) in files.iter().zip(&rebuild.folded) {
        summary.record_folded(*language);
        summary.nodes += folded.nodes;
        summary.edges += folded.edges;
    }

    // A scan that folded nothing is the failure this summary exists to expose —
    // it must not look like a successful scan of an empty repository.
    if summary.found_nothing_to_fold() || summary.truncated_by_cap {
        warn!(
            repository = %repository,
            revision = %revision.0,
            summary = %summary.headline(),
            "code-graph scan produced an incomplete graph"
        );
    } else {
        info!(
            repository = %repository,
            revision = %revision.0,
            summary = %summary.headline(),
            "code-graph scan complete"
        );
    }
    Ok(summary)
}

/// Drop the files that would blow the graph's symbol budget, and record why.
///
/// The one place the SYMBOL budget is spent, for the same reason [`Walk::flush`]
/// is the one place the FILE budget is spent: a second site would spend it under
/// a different rule.
///
/// Two independent limits, because there are two independent failures:
///
/// * a single file over [`SCAN_FILE_NODE_CAP`] is skipped and the walk continues
///   — it is generated code, and the rest of the repository is still worth
///   folding;
/// * once [`SCAN_NODE_BUDGET`] is spent the scan STOPS, keeping everything it
///   accepted so far.
///
/// Either outcome sets `truncated_by_cap`, which is what suppresses
/// [`codegraph::ScanCoverage`]'s retire pass. That is not incidental: a scan that
/// declined to fold a path has not established that the path is gone, so it must
/// not delete what a previous complete scan stored for it — the identical
/// property the file cap already had.
///
/// The two caps are parameters rather than the constants read directly, for the
/// reason [`collect_source_paths`] takes its `cap` the same way: a test must be
/// able to drive the budget without writing a 120,000-symbol fixture.
fn apply_node_budget(
    repository: RepositoryId,
    files: Vec<(String, String, Language)>,
    summary: &mut ScanSummary,
    file_node_cap: usize,
    node_budget: usize,
) -> anyhow::Result<Vec<(String, String, Language)>> {
    summary.node_budget = node_budget;
    summary.file_node_cap = file_node_cap;
    let mut kept = Vec::with_capacity(files.len());
    let mut weights = Vec::with_capacity(files.len());
    let mut spent = 0usize;
    for (relative, source, language) in files {
        // The parse that used to be `validate_file_graph`: same call, same
        // errors, and the node/edge counts kept rather than discarded. A
        // malformed file still fails the whole scan here, before anything is
        // written, so one bad file cannot leave the graph half-rebuilt.
        let measured = codegraph::measure_file_graph(repository, &relative, &source)?;
        weights.push(codegraph::FileNodeWeight {
            path: relative.clone(),
            nodes: measured.nodes,
        });
        if measured.nodes > file_node_cap {
            summary.files_skipped_oversized += 1;
            summary.truncated_by_cap = true;
            warn!(
                %repository,
                path = %relative,
                nodes = measured.nodes,
                cap = file_node_cap,
                "skipping a file that folds to more symbols than one file may contribute"
            );
            continue;
        }
        if spent + measured.nodes > node_budget {
            summary.truncated_by_node_budget = true;
            summary.truncated_by_cap = true;
            break;
        }
        spent += measured.nodes;
        kept.push((relative, source, language));
    }
    // Only the files actually weighed appear here — a budget stop leaves the
    // remainder unmeasured, and inventing a weight for them would be a number
    // nothing measured.
    summary.record_heaviest(weights);
    Ok(kept)
}

/// Why a directory cannot be scanned. Two distinct answers, because they call for
/// two distinct fixes: `NotARepository` is "run this inside a checkout", and
/// `ImplausibleRoot` is "this is not a project, it is your whole account".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanRootRefusal {
    /// `git rev-parse --show-toplevel` could not name a checkout for this path.
    NotARepository(PathBuf),
    /// A checkout WAS resolved, and it is the user's home directory or a
    /// filesystem root — a tree whose contents are an account, not a project.
    ImplausibleRoot(PathBuf),
}

impl std::fmt::Display for ScanRootRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotARepository(path) => {
                write!(f, "cannot scan {}: not a git repository", path.display())
            }
            Self::ImplausibleRoot(path) => write!(
                f,
                "refusing to scan {}: it is the home directory or a filesystem root, \
                 not a project checkout — indexing it folds IDE caches and editor \
                 history into the code graph",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ScanRootRefusal {}

/// Resolve `root` to the checkout's top-level directory, refusing the trees no
/// scan may walk.
///
/// Two gates, and the second is the belt to the first's braces:
///
/// 1. An ordinary directory is deliberately not treated as a repository:
///    recursively indexing a home or projects directory folds unrelated checkouts
///    into one enormous graph.
/// 2. A resolved root that IS the home directory or a filesystem root is refused
///    even though Git answered for it. Gate 1 already excludes `$HOME` on a
///    machine where it holds no `.git` — but a single stray `git init ~` reopens
///    the exact hole that produced a 510,904-node graph, three quarters of it
///    JetBrains' vendored Rust stdlib cache and Cursor's per-edit file history.
///    Nothing about that outcome should depend on a directory the user never
///    meant to initialise.
///
/// Fails closed: an ambiguous root is refused, never scanned.
pub fn resolve_scan_root(root: &Path) -> Result<PathBuf, ScanRootRefusal> {
    let Some(resolved) = git_toplevel(root) else {
        return Err(ScanRootRefusal::NotARepository(root.to_path_buf()));
    };
    if is_implausible_scan_root(&resolved) {
        return Err(ScanRootRefusal::ImplausibleRoot(resolved));
    }
    Ok(resolved)
}

/// Resolve `root` to the checkout's top-level directory, or `None` when it cannot
/// be scanned for either reason [`resolve_scan_root`] gives.
///
/// The `Option` form every existing caller wants — a repository id derivation, a
/// gateway's "is this a checkout" test. Callers that must EXPLAIN the refusal
/// (the scan itself, the watcher) use [`resolve_scan_root`] instead, so the log
/// does not report a home directory as "not a git repository".
#[must_use]
pub fn discover_repository_root(root: &Path) -> Option<PathBuf> {
    resolve_scan_root(root).ok()
}

/// A `git` invocation rooted at `root`, with the ambient repository-location
/// variables stripped from the child's environment.
///
/// **This is the hole `$HOME` came through.** `git rev-parse --show-toplevel`
/// does not ask "is this directory a repository" when `GIT_DIR` is set in the
/// environment: it answers with the *current directory*, whatever that directory
/// is. Verified on git 2.55.0 — in a `$HOME` holding no `.git` at all:
///
/// ```text
/// $ git rev-parse --show-toplevel
/// fatal: not a git repository (or any of the parent directories): .git
/// $ GIT_DIR=/some/other/repo/.git git rev-parse --show-toplevel
/// /Users/<user>
/// ```
///
/// So a daemon that inherited `GIT_DIR` — started from a git hook, `git rebase
/// -x`, `git bisect run`, or any shell where it was exported — makes
/// [`discover_repository_root`] answer `Some($HOME)` for a directory that is not
/// a repository, and the gate in [`scan_repository`] passes. `.gitignore` then
/// fails the same way and in the same direction: [`ignored_paths`] asks `git
/// check-ignore` in `$HOME`, git consults the *other* repository's rules, and
/// nothing under `Library/` is excluded, because that repository has never heard
/// of it.
///
/// The rest of the codebase already treats these as a hazard from the other
/// direction — `runtime::tools::shell::is_denied_env` refuses a model-supplied
/// `GIT_DIR`/`GIT_WORK_TREE`, and `runtime::tools::git::harden_git_env` strips the
/// execution hooks — but it deliberately lets git inherit the daemon's own
/// environment, which is correct for a command operating on a known checkout and
/// wrong for the one command whose entire job is to DECIDE which checkout that
/// is. This is that command, so it asks in a clean environment.
fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    for key in [
        // Where the repository is — the family that makes `--show-toplevel`
        // answer about somewhere else, or about nowhere.
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        // How far discovery may walk. A hostile or merely leftover value here
        // silently changes which directory is "the repository".
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    ] {
        command.env_remove(key);
    }
    command
}

/// Ask Git for `root`'s top-level directory, canonicalized. No policy — just the
/// question.
fn git_toplevel(root: &Path) -> Option<PathBuf> {
    let output = git_command(root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    if path.as_os_str().is_empty() {
        return None;
    }
    Some(path.canonicalize().unwrap_or(path))
}

/// True for a directory no code graph may ever be rooted at, however it was
/// reached: the user's home directory, or a path so shallow it can only be a
/// filesystem or account root (`/`, `/Users`, `/home`, `C:\`, `C:\Users`).
///
/// The depth rule is what covers the roots this process cannot name. `$HOME` is
/// read from the environment, which a daemon running as another user, a container
/// with no `HOME` set, or a Windows host may answer differently or not at all; a
/// path with fewer than two named components is an account root on every layout
/// there is, and no checkout lives at one.
fn is_implausible_scan_root(path: &Path) -> bool {
    if named_depth(path) < 2 {
        return true;
    }
    home_directory().is_some_and(|home| {
        // Compared canonically where possible: `$HOME` is frequently a symlink
        // (`/home/user` → `/data/home/user`, `/tmp` → `/private/tmp` on macOS),
        // and the resolved root has already been canonicalized.
        let home = home.canonicalize().unwrap_or(home);
        home == path
    })
}

/// How many *named* components a path has — the prefix and root separator do not
/// count, so `/` is 0, `/Users` is 1, and `/Users/dan` is 2.
fn named_depth(path: &Path) -> usize {
    path.components()
        .filter(|component| matches!(component, std::path::Component::Normal(_)))
        .count()
}

/// The user's home directory, from the environment.
///
/// `HOME` first, `USERPROFILE` for Windows — the same pair `crates/cli/src/update.rs`
/// and the daemon's policy root resolver already read, rather than a new
/// dependency for one lookup. Absent or empty yields `None`, and the depth rule
/// above still holds.
fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The working tree's `HEAD` commit as a [`GitRevision`], or the `"workdir"`
/// placeholder when Git is unavailable or `root` is not a repository. Shelling
/// out keeps this free of a Git library dependency.
///
/// Also the executor's re-scan gate: a run whose checkout has moved to a
/// revision the daemon has not folded warms the graph again, so a long-lived
/// daemon does not keep serving a repository map from whatever the tree looked
/// like at its first run.
///
/// Crate-visible because the `/update-docs` sweep labels its staleness findings
/// with the same revision the scan resolved links at (`crate::docs_job`).
#[must_use]
pub fn head_revision(root: &Path) -> GitRevision {
    let head = git_command(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    GitRevision(head.unwrap_or_else(|| "workdir".to_string()))
}

/// Collect up to `cap` `(repo-relative-path, source, language)` triples for the
/// source files under `root`. Which files those are is decided **once**, by
/// [`collect_source_paths`]; this only reads them.
///
/// Returns the [`ScanSummary`] fields the walk itself knows (seen, supported,
/// unsupported, ignored, pruned, cap); the caller fills in what the fold produced.
#[allow(clippy::type_complexity)]
fn collect_sources(
    root: &Path,
    cap: usize,
) -> std::io::Result<(Vec<(String, String, Language)>, ScanSummary)> {
    let (candidates, mut summary) = collect_source_paths(root, cap)?;
    let mut out = Vec::with_capacity(candidates.len());
    for (relative, language) in candidates {
        // A file that vanished between the walk and the read is not an error —
        // the tree is live. Only a genuine read failure of a present file is.
        match std::fs::read_to_string(root.join(&relative)) {
            Ok(source) => out.push((relative, source, language)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                summary.files_skipped_unreadable += 1;
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    Ok((out, summary))
}

/// How many paths one `git check-ignore` probe carries. The walk asks in chunks
/// so a single enormous directory cannot build an unbounded argument payload,
/// and so a level wider than this still costs a bounded amount of memory.
const IGNORE_PROBE_CHUNK: usize = 4096;

/// The repo-relative source paths under `root`, sorted and capped, each with the
/// language [`codegraph::language_for`] selected for it.
///
/// **The extension test is `codegraph::language_for` and nothing else.** It was
/// `ext == "rs"` here and again in [`is_candidate_path`], two lists that agreed
/// with the parser only by coincidence; when the parser grew Python and
/// TypeScript they would have kept rejecting both.
///
/// # The ignore rules run DURING the walk, not after it
///
/// They used to run after: the walk filled `cap` candidates and the filter then
/// removed the ignored ones. Adding the JavaScript and TypeScript grammars made
/// that fatal. `node_modules/` sorts before `src/`, `web/` and `app/`, and holds
/// far more than [`SCAN_FILE_CAP`] `.js` files, so a React checkout spent its
/// entire budget on dependency code, reached none of the application, and
/// [`codegraph::rebuild_repository`] then retired every real path as vanished —
/// an explicit `graph build` replacing a good graph with an empty one.
///
/// So an excluded path is now rejected before anything counts toward the cap,
/// and an excluded DIRECTORY is never descended into at all. The rules are the
/// checkout's own, asked of Git ([`ignored_paths`]) — never a reimplementation
/// of `.gitignore`, and never a second opinion about what is in scope.
///
/// # Why the walk is breadth-first
///
/// Asking Git once per directory would be a process spawn per directory: seconds
/// of subprocess on a monorepo, in a warm-up whose whole point is to be quick.
/// Level order lets one probe cover a whole level (in [`IGNORE_PROBE_CHUNK`]
/// chunks), so the cost is a spawn per level of depth — a handful. The order is
/// still fully deterministic (entries sorted per directory, directories walked in
/// discovery order), which is what the cap needs: it must truncate the same files
/// on every boot rather than rebuilding a different graph per `read_dir` order.
/// Breadth-first also makes a truncated graph a better cross-section of the
/// repository than one deep subtree.
fn collect_source_paths(
    root: &Path,
    cap: usize,
) -> std::io::Result<(Vec<(String, Language)>, ScanSummary)> {
    let mut walk = Walk::new(root, cap);
    let mut level = vec![root.to_path_buf()];
    while !level.is_empty() {
        if walk.files.len() >= cap {
            // Directories still unwalked with the budget already spent: this
            // graph is a truncation of the repository, and must say so.
            walk.summary.truncated_by_cap = true;
            break;
        }
        for dir in level {
            if walk.summary.truncated_by_cap {
                break;
            }
            let mut entries: Vec<_> = std::fs::read_dir(&dir)?.collect::<Result<_, _>>()?;
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let file_type = entry.file_type()?;
                if file_type.is_dir() {
                    if is_excluded_name(&name) {
                        walk.summary.dirs_pruned += 1;
                        continue;
                    }
                    walk.queue_dir(path);
                    continue;
                }
                if !file_type.is_file() || is_excluded_name(&name) {
                    continue;
                }
                walk.queue_file(&path);
            }
            if walk.queued() >= IGNORE_PROBE_CHUNK {
                walk.flush();
            }
        }
        walk.flush();
        level = std::mem::take(&mut walk.next_level);
    }
    Ok((walk.files, walk.summary))
}

/// Directory (and dot-file) names the code graph never walks, watches or folds,
/// whatever the ignore rules say.
///
/// The ignore rules are the authority on scope and this is not a second one: it
/// is a floor under them. [`ignored_paths`] is best-effort by design — no Git, a
/// broken index, any spawn failure and it excludes *nothing* — and in exactly
/// that degraded state the defect this list guards against comes straight back.
/// `node_modules` earns its place because it is the one directory name that is
/// never hand-written source in any project, and because it is the tree that
/// actually ate the cap. `target` and the dot-directories were already here.
/// Nothing else is added: `dist`, `build` and `vendor` are all real source
/// directories in some checkout, and a name list that guesses is how the walk
/// and the parser came to disagree in the first place. (The workspace parse in
/// `codypendent_knowledge::adapter` already excludes `node_modules` by name, so
/// this is the codebase's existing opinion rather than a new one.)
///
/// # The IDE cache roots
///
/// `Library`, `Caches` and `AppData` were added for the same reason
/// `node_modules` is here, established the same way: they are the trees that
/// actually ate the graph. A measured user database held 386,572 nodes — 76% of
/// the whole graph — under
/// `Library/Caches/JetBrains/RustRover2026.1/intellij-rust/stdlib/…/vendor/windows-sys/…`
/// and `Library/Application Support/Cursor/User/History/…`: a vendored Rust
/// stdlib an IDE unpacked, and one snapshot per keystroke-save an editor kept.
/// Neither is the user's code, neither is `.gitignore`d (there is no checkout to
/// hold the rules), and the walk descended into both.
///
/// These are the platform cache ROOTS by their exact, capitalised, OS-defined
/// names — `~/Library` on macOS, `AppData` on Windows, and `Caches` wherever a
/// tree nests one. `.cache` (Linux/XDG) needs no entry: it is dot-prefixed and
/// the first clause already has it. The residual cost is a project that keeps
/// hand-written source in a directory literally named `Library` (a Unity
/// checkout's `Library/` is itself a generated asset cache, so that one is a
/// win); that is the trade, and it is the same trade `target` and `node_modules`
/// already make.
///
/// `vendor` is deliberately NOT here. It is real, committed source in real
/// checkouts — this codebase's own graph held a legitimate
/// `Typhon/tyc/vendor/ruff_python_ast` — and the vendored tree that caused the
/// incident sat *under* `Library/Caches`, which the walk now never descends into
/// at all. Pruning the cache root already excludes everything below it, so a
/// `vendor` rule would buy nothing and cost real source. The heavy files a
/// vendored tree inside a genuine checkout can still produce are what
/// [`SCAN_FILE_NODE_CAP`] is for.
fn is_excluded_name(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name,
            "target" | "node_modules" | "Library" | "Caches" | "AppData"
        )
}

/// The walk's mutable state.
///
/// A struct rather than locals because the ignore probe is flushed from two
/// places — a full chunk and the end of a level — and both must apply the same
/// filter, count the same way, and spend the cap in the same place. That "same
/// place" is [`Walk::flush`] and only there: a file counts toward the cap after
/// the ignore rules have accepted it, never before.
struct Walk {
    root: PathBuf,
    cap: usize,
    /// Accepted source files, in walk order. Never longer than `cap`.
    files: Vec<(String, Language)>,
    /// Accepted directories, forming the next level.
    next_level: Vec<PathBuf>,
    /// Discovered but not yet asked about.
    probe_dirs: Vec<(String, PathBuf)>,
    probe_files: Vec<(String, Language)>,
    summary: ScanSummary,
}

impl Walk {
    fn new(root: &Path, cap: usize) -> Self {
        Self {
            root: root.to_path_buf(),
            cap,
            files: Vec::new(),
            next_level: Vec::new(),
            probe_dirs: Vec::new(),
            probe_files: Vec::new(),
            summary: ScanSummary {
                file_cap: cap,
                ..ScanSummary::default()
            },
        }
    }

    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    fn queue_dir(&mut self, path: PathBuf) {
        let relative = self.relative(&path);
        self.probe_dirs.push((relative, path));
    }

    fn queue_file(&mut self, path: &Path) {
        self.summary.files_seen += 1;
        let Some(language) = codegraph::language_for(path) else {
            self.summary.record_unsupported(path);
            return;
        };
        self.summary.files_supported += 1;
        self.probe_files.push((self.relative(path), language));
    }

    fn queued(&self) -> usize {
        self.probe_dirs.len() + self.probe_files.len()
    }

    /// Ask the checkout's ignore rules about everything queued, then accept what
    /// survives — one `git check-ignore` for the whole chunk.
    fn flush(&mut self) {
        if self.queued() == 0 {
            return;
        }
        let dirs = std::mem::take(&mut self.probe_dirs);
        let files = std::mem::take(&mut self.probe_files);
        let probe: Vec<String> = dirs
            .iter()
            .map(|(relative, _)| relative.clone())
            .chain(files.iter().map(|(relative, _)| relative.clone()))
            .collect();
        let ignored = ignored_paths(&self.root, &probe);
        for (relative, language) in files {
            if ignored.contains(&relative) {
                self.summary.files_skipped_ignored += 1;
                continue;
            }
            if self.files.len() >= self.cap {
                self.summary.truncated_by_cap = true;
                break;
            }
            self.files.push((relative, language));
        }
        for (relative, path) in dirs {
            if ignored.contains(&relative) {
                self.summary.dirs_pruned += 1;
                continue;
            }
            self.next_level.push(path);
        }
    }
}

/// Which of `relative` the checkout's ignore rules exclude, asked of Git itself.
///
/// Reimplementing `.gitignore` (nested files, negations, `core.excludesFile`,
/// `.git/info/exclude`) is a well-known source of quiet divergence, and this
/// crate already shells out to `git` for `rev-parse`. Best-effort by design: no
/// Git, a non-repository, or any spawn failure yields an EMPTY set, so the
/// caller behaves exactly as it did before this filter existed rather than
/// indexing nothing.
///
/// `--stdin -z` on both directions so a path containing a newline or a quote is
/// passed and returned verbatim. Stdin is written from a helper thread: the
/// payload can exceed a pipe buffer, and writing it inline while the child
/// writes its own output back would deadlock.
fn ignored_paths(root: &Path, relative: &[String]) -> HashSet<String> {
    if relative.is_empty() {
        return HashSet::new();
    }
    let mut payload = Vec::new();
    for path in relative {
        payload.extend_from_slice(path.as_bytes());
        payload.push(0);
    }
    // Asked in the same scrubbed environment [`git_command`] establishes: an
    // inherited `GIT_DIR` makes `check-ignore` answer from ANOTHER repository's
    // rules, which is silently "nothing here is ignored" for every path in this
    // one — the second half of the same defect.
    let child = git_command(root)
        .args(["check-ignore", "--stdin", "-z"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        return HashSet::new();
    };
    let Some(mut stdin) = child.stdin.take() else {
        return HashSet::new();
    };
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&payload);
    });
    let output = child.wait_with_output();
    let _ = writer.join();
    let Ok(output) = output else {
        return HashSet::new();
    };
    // Exit 1 means "nothing was ignored"; 128 means Git could not answer. Both
    // leave an empty set, which is the same as "ignore nothing".
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|chunk| !chunk.is_empty())
        .filter_map(|chunk| std::str::from_utf8(chunk).ok())
        .map(str::to_owned)
        .collect()
}

// --------------------------------------------------------------------------
// The live code graph — a debounced, incremental filesystem watcher
// --------------------------------------------------------------------------

/// How long the watcher waits for the filesystem to go quiet before folding a
/// batch. An editor save is several events (write, rename, chmod) and a `git
/// checkout` is thousands; without a debounce every keystroke-triggered
/// autosave would be its own reparse.
pub const WATCH_DEBOUNCE: Duration = Duration::from_millis(400);

/// Hard ceiling on one debounce window, so a tool that writes continuously
/// (`cargo watch`, a formatter loop) cannot postpone the fold forever by never
/// letting the tree fall quiet.
pub const WATCH_MAX_WINDOW: Duration = Duration::from_secs(3);

/// How many distinct changed files one batch folds individually. Past this a
/// batch is a branch switch or a rebase, not an edit, and one full rescan is
/// both cheaper and more correct (it also retires files deleted by the switch).
pub const WATCH_BATCH_CAP: usize = 64;

/// Minimum interval between two overflow-triggered full rescans, so a long
/// rebase cannot chain them back to back.
pub const WATCH_FULL_RESCAN_COOLDOWN: Duration = Duration::from_secs(30);

/// Depth of the channel from the notify thread to the debouncer. Bounded: the
/// notify callback must never block or grow without limit, so an overrun is
/// dropped and recorded, and the batch it belongs to degrades to a full rescan.
const WATCH_CHANNEL_CAP: usize = 4096;

#[derive(Default)]
struct ImporterQueue {
    paths: VecDeque<String>,
    queued: HashSet<String>,
}

impl ImporterQueue {
    fn extend(&mut self, paths: impl IntoIterator<Item = String>) {
        for path in paths {
            if self.queued.insert(path.clone()) {
                self.paths.push_back(path);
            }
        }
    }

    fn take_batch(&mut self) -> Vec<String> {
        let mut paths = Vec::with_capacity(WATCH_BATCH_CAP);
        while paths.len() < WATCH_BATCH_CAP {
            let Some(path) = self.paths.pop_front() else {
                break;
            };
            self.queued.remove(&path);
            paths.push(path);
        }
        paths
    }

    fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    fn clear(&mut self) {
        self.paths.clear();
        self.queued.clear();
    }
}

async fn queue_affected_importers(
    pool: &SqlitePool,
    repository: RepositoryId,
    changed_paths: &[String],
    importers: &mut ImporterQueue,
) -> Result<(), codegraph::CodeGraphError> {
    let mut after = None;
    while !changed_paths.is_empty() {
        let page = codegraph::affected_importers_page(
            pool,
            repository,
            changed_paths,
            after.as_deref(),
            WATCH_BATCH_CAP,
        )
        .await?;
        after = page.paths.last().cloned();
        importers.extend(page.paths);
        if !page.truncated || after.is_none() {
            break;
        }
    }
    Ok(())
}

async fn indexed_source_matches(
    pool: &SqlitePool,
    repository: RepositoryId,
    path: &str,
    digest: &[u8; 32],
) -> Result<bool, codegraph::CodeGraphError> {
    let evidence: Option<String> = sqlx::query_scalar(
        "SELECT e.evidence_artifact FROM code_edges e JOIN code_nodes n ON n.id = e.from_node \
         WHERE n.repository = ? AND n.source_path = ? AND e.evidence_kind = 'syntax_inferred' \
         AND e.evidence_artifact IS NOT NULL LIMIT 1",
    )
    .bind(repository.to_string())
    .bind(path)
    .fetch_optional(pool)
    .await?;
    Ok(evidence
        .map(|raw| serde_json::from_str::<codypendent_knowledge::EvidenceRef>(&raw))
        .transpose()?
        .is_some_and(|evidence| {
            matches!(
                evidence,
                codypendent_knowledge::EvidenceRef::Artifact { artifact, .. }
                    if artifact.sha256 == hex::encode(digest)
            )
        }))
}

/// A live per-repository code-graph watcher. Dropping it stops the background
/// task and, with it, the underlying `notify` watcher and its thread.
pub struct RepositoryWatcher {
    repository: RepositoryId,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for RepositoryWatcher {
    fn drop(&mut self) {
        self.task.abort();
        debug!(repository = %self.repository, "code-graph watcher stopped");
    }
}

/// Arm a debounced, incremental code-graph watcher over `root`.
///
/// This is the wire outcome 14 needs: without it the graph only ever moves when
/// git `HEAD` moves, so a file edited during a session — including by the
/// agent's own `edit_file` — never enters the graph and every later turn's
/// repository map describes the pre-edit tree (2026-08-13 review, F2).
///
/// Bounded on every axis, because this runs for the daemon's whole life against
/// a tree a build system is also writing to:
///
/// * `target/`, `.git/`, any other dot-directory, and every `.gitignore`d
///   top-level directory are never WATCHED at all — one `inotify` watch is
///   taken per directory, so filtering their events afterwards would still
///   register thousands of kernel watches for build output;
/// * of what is watched, only files [`codegraph::language_for`] recognises —
///   plus a newly created top-level DIRECTORY, which nothing else can report
///   (see [`is_new_top_level_directory`]), plus `.gitignore` itself, whose edit
///   re-scopes files that emit no event (see [`is_ignore_rules_file`]) — reach
///   the channel, filtered in the notify callback, before the queue;
/// * `.gitignore` is applied per batch, by asking Git (see [`ignored_paths`]),
///   and a change to it schedules a full rescan;
/// * events are debounced by [`WATCH_DEBOUNCE`] and capped at [`WATCH_MAX_WINDOW`];
/// * a batch over [`WATCH_BATCH_CAP`] files (a branch switch), or one that armed
///   a new subtree, collapses to ONE full rescan, rate-limited by
///   [`WATCH_FULL_RESCAN_COOLDOWN`];
/// * everything else is an incremental per-file reparse — never a
///   clear-and-rebuild, which at editor-save frequency would be ruinous.
pub fn arm_watcher(
    pool: SqlitePool,
    repository: RepositoryId,
    root: &Path,
) -> anyhow::Result<RepositoryWatcher> {
    // Watch the checkout's TOP LEVEL, and derive relative paths from it — the
    // same root `scan_repository` resolves. Watching the run's directory instead
    // would stamp `source_path` values the full scan never writes, so a reparse
    // would create a second set of nodes rather than updating the first.
    //
    // Refused rather than defaulted. This used to be
    // `discover_repository_root(root).unwrap_or_else(|| root.to_path_buf())`, so
    // a root the SCAN would have refused was still armed — a recursive watch over
    // an arbitrary directory, folding it file by file through `apply_batch` with
    // no cap of any kind, and with `ignored_paths` degrading to "nothing is
    // ignored" because there is no checkout to ask. Both current callers only
    // reach here after a successful `scan_repository`, so the fallback was
    // unreachable; it was also the single line standing between a third caller
    // and the whole defect, and a watcher is not a thing to arm on a guess.
    let root = resolve_scan_root(root)?;
    let (tx, rx) = mpsc::channel::<PathBuf>(WATCH_CHANNEL_CAP);
    let filter_root = root.clone();
    // The notify callback runs on notify's own thread: `try_send` is the only
    // correct send here — it never blocks that thread and never queues without
    // bound. A full channel therefore DROPS the path, which would be a silently
    // missed edit; the drop is counted instead, and the debouncer turns any
    // non-zero count into a full rescan.
    let dropped = Arc::new(AtomicUsize::new(0));
    let dropped_by_notify = Arc::clone(&dropped);
    let mut watcher = codegraph::watcher(move |paths: Vec<PathBuf>| {
        for path in paths {
            // Three things must reach the debouncer: a file the graph could
            // hold; a NEW top-level directory — which holds no extension, so the
            // language filter discards it, and the debounce loop is the only
            // caller of `arm_source_subtrees` (2026-08-13 review, codegraph F8);
            // and a change to the ignore RULES, which changes the answer to
            // "what belongs in the graph" for files that emit no event of their
            // own (see [`is_ignore_rules_file`]).
            // Everything else is still rejected here, before the bounded queue,
            // because a full queue drops paths and escalates the batch to a full
            // rescan.
            if !is_candidate_path(&filter_root, &path)
                && !is_new_top_level_directory(&filter_root, &path)
                && !is_ignore_rules_file(&filter_root, &path)
            {
                continue;
            }
            if tx.try_send(path).is_err() {
                dropped_by_notify.fetch_add(1, Ordering::Relaxed);
            }
        }
    })?;
    let mut watched = HashSet::new();
    arm_source_subtrees(&mut watcher, &root, &mut watched);

    let task = tokio::spawn(async move {
        // The watcher lives exactly as long as this task: aborting the task on
        // `RepositoryWatcher::drop` drops it here, which joins notify's thread.
        watch_loop(pool, repository, root, rx, dropped, watcher, watched).await;
    });
    info!(%repository, "code-graph watcher armed");
    Ok(RepositoryWatcher { repository, task })
}

/// True for a path the code graph could possibly hold: a file in a language
/// [`codegraph::language_for`] recognises that is not inside `target/`, `.git/`,
/// or any other dot-directory **within the checkout**. Cheap — it runs on
/// notify's thread for every raw event, including the thousands a build produces.
///
/// The language test goes through `language_for` for the same reason
/// [`collect_source_paths`] does: the watcher's idea of "worth folding" and the
/// parser's idea of "can parse" must be one question. They were two, and the
/// watcher's answer was `*.rs`.
///
/// The scan is deliberately relative to `root`: the checkout's own ancestors are
/// none of this filter's business. Testing the absolute path instead rejected
/// every file in a repository whose path happened to contain a dot-directory —
/// `/tmp/.tmpXk9/src/lib.rs`, or anything under `~/.local/share`, or a checkout
/// inside a dot-prefixed workspace directory — and the watcher then silently
/// folded nothing at all.
fn is_candidate_path(root: &Path, path: &Path) -> bool {
    if codegraph::language_for(path).is_none() {
        return false;
    }
    within_walked_tree(root, path)
}

/// True when every component of `path` below `root` is one the walk accepts —
/// the same [`is_excluded_name`] test the full scan applies to each entry, so the
/// watcher and the scan cannot disagree about which paths exist. They must not:
/// a file only the watcher accepts is folded by a batch and retired by the very
/// next full scan, forever.
///
/// Callers pass the file itself; [`is_ignore_rules_file`] passes the parent
/// directory instead, because `.gitignore` is dot-prefixed and would fail its own
/// name test.
///
/// Deliberately relative to `root`: the checkout's own ancestors are none of this
/// filter's business. Testing the absolute path instead rejected every file in a
/// repository whose path happened to contain a dot-directory —
/// `/tmp/.tmpXk9/src/lib.rs`, or anything under `~/.local/share`, or a checkout
/// inside a dot-prefixed workspace directory — and the watcher then silently
/// folded nothing at all.
fn within_walked_tree(root: &Path, path: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    !relative.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        name != "." && name != ".." && is_excluded_name(&name)
    })
}

/// The file whose CONTENT is the checkout's ignore rules.
///
/// Its own event must reach the debouncer, because a `.gitignore` edit changes
/// which files belong in the graph without touching one of them: a file that has
/// just become ignored emits no event of its own, ever. Without this the graph
/// kept serving excluded code indefinitely — a later source event could not
/// repair it either, since [`apply_batch`] *skips* an ignored path rather than
/// retiring what it already stored for it. Only a full rescan can, and the
/// debounce loop schedules one when this predicate matches.
///
/// Exactly one file name, not a pattern: this is the one gap in the notify
/// filter, and everything else — including any other dot-file — is still
/// rejected before the bounded queue. `.git/info/exclude` and
/// `core.excludesFile` change the same rules but live outside every watch, so no
/// event for them exists to admit.
fn is_ignore_rules_file(root: &Path, path: &Path) -> bool {
    if path.file_name().and_then(|name| name.to_str()) != Some(".gitignore") {
        return false;
    }
    // The DIRECTORY is what gets scope-tested: `.gitignore` is itself dot-
    // prefixed, so testing the file's own name against the exclusion rule would
    // reject every one of them. A `.gitignore` inside `target/` or under a
    // dot-directory is still not this repository's ignore rules.
    let Some(parent) = path.parent() else {
        return false;
    };
    within_walked_tree(root, parent)
}

/// True for a directory that is an **immediate child of the checkout root** and
/// could hold source — the one event [`is_candidate_path`] must not swallow even
/// though the graph will never hold the path itself.
///
/// A directory carries no source extension, so the language filter dropped its
/// creation event before the channel; and because the debounce loop is what calls
/// [`arm_source_subtrees`], nothing ever armed the new directory. A package added
/// mid-session then stayed outside every watch until some unrelated accepted
/// event started a batch — and its already-written files needed a *second* edit
/// after that before they folded (2026-08-13 review, codegraph F8).
///
/// Deliberately only the root's own children. Deeper directories are already
/// inside a recursive watch, so their appearance needs no re-arming, and the
/// restriction is what keeps the `is_dir` stat off the hot path: this runs on
/// notify's thread for every raw event, and the thousands a build or a checkout
/// produces are all more than one component deep. The name test comes first, so a
/// dot-directory or `target/` costs no syscall at all.
fn is_new_top_level_directory(root: &Path, path: &Path) -> bool {
    if path.parent() != Some(root) {
        return false;
    }
    let Some(name) = path.file_name() else {
        return false;
    };
    if is_excluded_name(&name.to_string_lossy()) {
        return false;
    }
    path.is_dir()
}

/// Watch `root` itself (non-recursively) plus every immediate child directory
/// that could hold source, recursively — skipping `target/`, `.git/`, any other
/// dot-directory, and anything `.gitignore` excludes.
///
/// One `inotify` watch is taken per directory, so a recursive watch on the root
/// would register thousands for build output that can only ever produce events
/// [`is_candidate_path`] discards. `already` carries the set arming has covered,
/// so re-calling this only adds newly appeared directories. Returns the
/// directories this call newly DISCOVERED — empty on every batch that changed
/// nothing, which is almost all of them. Best-effort per directory: one
/// unwatchable path is logged and never fatal, and it is still reported, because
/// its existing contents went unannounced either way and only the caller's
/// rescan can fold them.
fn arm_source_subtrees(
    watcher: &mut codegraph::GraphWatcher,
    root: &Path,
    already: &mut HashSet<PathBuf>,
) -> Vec<PathBuf> {
    let mut armed = Vec::new();
    // The root is watched non-recursively so a NEW top-level directory (a new
    // crate) is noticed at all; the sweep below then arms it on the next batch.
    if already.insert(root.to_path_buf()) {
        if let Err(error) = watcher.watch_subtree(root, false) {
            warn!(root = %root.display(), %error, "could not watch the repository root");
        }
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return armed;
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_excluded_name(&name) {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            candidates.push((name.into_owned(), entry.path()));
        }
    }
    let relative: Vec<String> = candidates.iter().map(|(name, _)| name.clone()).collect();
    let ignored = ignored_paths(root, &relative);
    for (name, path) in candidates {
        if ignored.contains(&name) || !already.insert(path.clone()) {
            continue;
        }
        if let Err(error) = watcher.watch_subtree(&path, true) {
            warn!(path = %path.display(), %error, "could not watch a source subtree");
        }
        armed.push(path);
    }
    armed
}

/// Drain events, debounce them into batches, and fold each batch into the graph.
#[allow(clippy::too_many_arguments)]
async fn watch_loop(
    pool: SqlitePool,
    repository: RepositoryId,
    root: PathBuf,
    mut rx: mpsc::Receiver<PathBuf>,
    dropped: Arc<AtomicUsize>,
    mut watcher: codegraph::GraphWatcher,
    mut watched: HashSet<PathBuf>,
) {
    let mut last_full_rescan = Instant::now() - WATCH_FULL_RESCAN_COOLDOWN;
    // The bytes last folded for each path, so a repeated event for an unchanged
    // file costs a hash instead of a parse and a transaction. Belt and braces
    // over the event-kind filter: any future event storm degrades to a read.
    let mut folded: HashMap<String, [u8; 32]> = HashMap::new();
    let mut importers = ImporterQueue::default();
    let mut retry_paths = HashSet::new();
    loop {
        let first = if importers.is_empty() && retry_paths.is_empty() {
            rx.recv().await
        } else {
            tokio::time::timeout(WATCH_DEBOUNCE, rx.recv())
                .await
                .unwrap_or(None)
        };
        if first.is_none() && importers.is_empty() && retry_paths.is_empty() {
            break;
        }
        let mut pending: HashSet<PathBuf> = HashSet::new();
        pending.extend(first);
        pending.extend(retry_paths.drain());
        // Collect until the tree goes quiet for WATCH_DEBOUNCE, or the window
        // ceiling is reached — whichever comes first.
        let window_ends = Instant::now() + WATCH_MAX_WINDOW;
        let closed = if pending.is_empty() {
            rx.is_closed()
        } else {
            loop {
                let quiet =
                    WATCH_DEBOUNCE.min(window_ends.saturating_duration_since(Instant::now()));
                if quiet.is_zero() {
                    break false;
                }
                match tokio::time::timeout(quiet, rx.recv()).await {
                    Ok(Some(path)) => {
                        pending.insert(path);
                    }
                    Ok(None) => break true,
                    Err(_elapsed) => break false,
                }
            }
        };

        // A new top-level directory (a package added mid-session) is covered by
        // no recursive watch yet — the root's own non-recursive watch is what
        // reported it, which is why [`is_new_top_level_directory`] lets a
        // directory event past the filter at all. One `read_dir` of the root per
        // batch closes that gap, and it runs BEFORE the fold decision below:
        // every file already inside that directory was written while nothing
        // watched it, so no event for it exists or ever will, and a rescan is the
        // only thing that can see them.
        let armed = arm_source_subtrees(&mut watcher, &root, &mut watched);

        // Four reasons to prefer one full rebuild over per-file folds: a batch
        // this large is a branch switch or a rebase (rebuilding is both cheaper
        // and retires files the switch deleted); the notify queue overran and
        // this batch is missing paths it will never see again; a subtree was
        // just armed, whose existing contents no event ever announced; or the
        // ignore RULES changed, which silently re-scopes files that emit no
        // event of their own — a per-file fold cannot see either direction of
        // that change, because `apply_batch` skips an ignored path rather than
        // retiring what the graph already holds for it.
        let lost = dropped.swap(0, Ordering::Relaxed);
        let ignore_rules_changed = pending.iter().any(|path| is_ignore_rules_file(&root, path));
        let wants_rebuild = lost > 0
            || pending.len() > WATCH_BATCH_CAP
            || !armed.is_empty()
            || ignore_rules_changed;

        // One writer at a time: the same gate a full warm-up holds, so a batch
        // can never interleave with a clear-and-rebuild (F6).
        let guard = lock_repository(repository).await;
        let rebuilt = if wants_rebuild && last_full_rescan.elapsed() >= WATCH_FULL_RESCAN_COOLDOWN {
            info!(
                %repository,
                changed = pending.len(),
                dropped_events = lost,
                newly_watched = armed.len(),
                ignore_rules_changed,
                "code-graph watcher: bulk change, rebuilding"
            );
            match scan_repository(&pool, repository, &root).await {
                Ok(_summary) => {
                    last_full_rescan = Instant::now();
                    importers.clear();
                    folded.clear();
                    true
                }
                Err(error) => {
                    warn!(%repository, %error, "code-graph rebuild failed");
                    false
                }
            }
        } else {
            false
        };
        let deferred_rebuild = wants_rebuild && !rebuilt;
        if !rebuilt {
            // Within the rescan cooldown, or the rebuild failed. Fold what this
            // batch DOES name rather than discarding it — bounded per-file work
            // that is still correct for every path the queue delivered.
            if wants_rebuild {
                warn!(
                    %repository,
                    changed = pending.len(),
                    dropped_events = lost,
                    newly_watched = armed.len(),
                    ignore_rules_changed,
                    "code-graph watcher: rebuild deferred by the cooldown; folding this batch incrementally"
                );
            }
            if !apply_batch(
                &pool,
                repository,
                &root,
                &pending,
                &mut folded,
                &mut importers,
            )
            .await
            {
                retry_paths.extend(pending);
            }
        }
        drop(guard);

        // A dropped notify event names no path, and neither does a subtree armed
        // over files that were written before it existed, so folding the paths
        // that did arrive cannot repair the graph in either case. Previously a
        // cooldown-deferred rebuild forgot `lost` here and waited for an
        // unrelated future filesystem event; if the tree then stayed quiet the
        // graph remained stale forever. Wait
        // outside the repository lock and perform the promised authoritative
        // rebuild even when no further event arrives. Events accumulated during
        // the wait remain in the bounded channel and are folded afterwards.
        if deferred_rebuild {
            let remaining = WATCH_FULL_RESCAN_COOLDOWN.saturating_sub(last_full_rescan.elapsed());
            if !remaining.is_zero() {
                tokio::time::sleep(remaining).await;
            }
            let guard = lock_repository(repository).await;
            match scan_repository(&pool, repository, &root).await {
                Ok(_summary) => {
                    last_full_rescan = Instant::now();
                    importers.clear();
                    folded.clear();
                    info!(%repository, "code-graph watcher: completed deferred rebuild");
                }
                Err(error) => {
                    // Preserve the need for a rebuild. The next queued event (or
                    // producer overflow) will retry instead of treating the
                    // missing paths as repaired.
                    dropped.fetch_add(lost.max(1), Ordering::Relaxed);
                    warn!(%repository, %error, "code-graph deferred rebuild failed");
                }
            }
            drop(guard);
        }

        if closed && importers.is_empty() && retry_paths.is_empty() {
            break;
        }
    }
}

/// Reparse (or retire) each changed path, incrementally.
///
/// A batch also carries the two paths the notify filter admits for their *side
/// effects* rather than their content: a new top-level directory (so
/// `arm_source_subtrees` runs) and `.gitignore` (so the batch escalates to a full
/// rescan). Both have already done their job by the time the batch reaches here,
/// so neither is offered to the parser — the same [`codegraph::language_for`]
/// question the walk and the watcher both ask decides it, rather than a third
/// rule, an `is_dir` stat, or a failed read.
async fn apply_batch(
    pool: &SqlitePool,
    repository: RepositoryId,
    root: &Path,
    pending: &HashSet<PathBuf>,
    folded: &mut HashMap<String, [u8; 32]>,
    importers: &mut ImporterQueue,
) -> bool {
    let mut relative: Vec<String> = pending
        .iter()
        .filter(|path| codegraph::language_for(path).is_some())
        .filter_map(|path| repo_relative(root, path))
        .collect();
    relative.sort();
    if relative.is_empty() && importers.is_empty() {
        return true;
    }
    // `.gitignore` is asked once per batch, not once per file — and the batch's
    // revision stamp with it, in the same hop off the runtime. Both shell out to
    // Git, and `working_tree_revision` now runs `git status`, which stats the
    // whole worktree; blocking a runtime worker on that once per debounce window
    // is exactly the stall `spawn_blocking` exists to avoid.
    //
    // Every node this batch writes over a dirty tree is stamped
    // `<head>+workdir`, so the graph says out loud that it is describing an
    // uncommitted tree rather than claiming the symbol was seen at that commit.
    // Nothing filters on the revision column; the TUI's edge table and `graph.*`
    // answers print it.
    let ignore_root = root.to_path_buf();
    let probe = relative.clone();
    let (ignored, revision) = tokio::task::spawn_blocking(move || {
        (
            ignored_paths(&ignore_root, &probe),
            working_tree_revision(&ignore_root),
        )
    })
    .await
    // A panicked blocking task leaves nothing ignored and the placeholder
    // revision — the same conservative answers `ignored_paths` degrades to.
    .unwrap_or_else(|_| (HashSet::new(), GitRevision("workdir".to_string())));
    let mut changes = Vec::new();
    for path in relative {
        if ignored.contains(&path) {
            continue;
        }
        let absolute = root.join(&path);
        match std::fs::read_to_string(&absolute) {
            Ok(source) => {
                let digest: [u8; 32] = Sha256::digest(source.as_bytes()).into();
                if folded.get(&path) == Some(&digest) {
                    continue;
                }
                changes.push((path, Some((source, digest))));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                changes.push((path, None));
            }
            Err(error) => debug!(%repository, path, %error, "unreadable changed file; skipped"),
        }
    }

    let changed_paths: Vec<String> = changes.iter().map(|(path, _)| path.clone()).collect();
    // Retiring a callee also removes its inbound edges. Capture dependents
    // first, including imports whose target has not been defined yet.
    if let Err(error) = queue_affected_importers(pool, repository, &changed_paths, importers).await
    {
        warn!(%repository, %error, "code-graph importer lookup failed; retrying the unchanged batch");
        return false;
    }

    let mut succeeded = true;
    let mut reparsed = 0usize;
    let mut retired = 0usize;
    for (path, source) in changes {
        match source {
            Some((source, digest)) => {
                match codegraph::upsert_file_graph(pool, repository, &revision, &path, &source)
                    .await
                {
                    Ok(_) => {
                        // Bounded: a repository past the scan cap would otherwise
                        // grow this map without limit over the daemon's life.
                        if folded.len() >= SCAN_FILE_CAP {
                            folded.clear();
                        }
                        folded.insert(path.clone(), digest);
                        reparsed += 1;
                        if changed_paths.len() > 1 {
                            importers.extend([path]);
                        }
                    }
                    Err(error) => {
                        succeeded &= !matches!(error, codegraph::CodeGraphError::Sqlx(_));
                        warn!(%repository, path, %error, "code-graph reparse failed");
                    }
                }
            }
            None => {
                folded.remove(&path);
                match codegraph::remove_file_graph(pool, repository, &path).await {
                    Ok(removed) if removed > 0 => retired += 1,
                    Ok(_) => {}
                    Err(error) => {
                        succeeded = false;
                        warn!(%repository, path, %error, "code-graph retire failed");
                    }
                }
            }
        }
    }

    let paths = importers.take_batch();
    let ignore_root = root.to_path_buf();
    let probe = paths.clone();
    let ignored = tokio::task::spawn_blocking(move || ignored_paths(&ignore_root, &probe))
        .await
        .unwrap_or_default();
    let mut reconciled = 0usize;
    for path in paths {
        let absolute = root.join(&path);
        if ignored.contains(&path) || !is_candidate_path(root, &absolute) {
            continue;
        }
        let source = match absolute.canonicalize() {
            Ok(canonical) if canonical.starts_with(root) => std::fs::read_to_string(canonical),
            Ok(_) => continue,
            Err(error) => Err(error),
        };
        match source {
            Ok(source) => {
                let digest: [u8; 32] = Sha256::digest(source.as_bytes()).into();
                let unchanged = match folded.get(&path) {
                    Some(previous) => Ok(previous == &digest),
                    None => indexed_source_matches(pool, repository, &path, &digest).await,
                };
                // A queued importer may itself have been edited before its
                // notify event arrived. Capture its callers before replacing it.
                let capture = match unchanged {
                    Ok(true) => Ok(()),
                    Ok(false) => {
                        queue_affected_importers(
                            pool,
                            repository,
                            std::slice::from_ref(&path),
                            importers,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                if let Err(error) = capture {
                    importers.extend([path.clone()]);
                    warn!(%repository, path, %error, "code-graph queued source lookup failed; retrying");
                    continue;
                }
                match codegraph::reconcile_imports_for_file(
                    pool, repository, &revision, &path, &source,
                )
                .await
                {
                    Ok(_) => {
                        if folded.len() >= SCAN_FILE_CAP {
                            folded.clear();
                        }
                        folded.insert(path, digest);
                        reconciled += 1;
                    }
                    Err(error) => {
                        if matches!(error, codegraph::CodeGraphError::Sqlx(_)) {
                            importers.extend([path.clone()]);
                        }
                        warn!(%repository, path, %error, "code-graph importer refresh failed");
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(error) = queue_affected_importers(
                    pool,
                    repository,
                    std::slice::from_ref(&path),
                    importers,
                )
                .await
                {
                    importers.extend([path.clone()]);
                    warn!(%repository, path, %error, "code-graph queued deletion lookup failed; retrying");
                    continue;
                }
                folded.remove(&path);
                if let Err(error) = codegraph::remove_file_graph(pool, repository, &path).await {
                    importers.extend([path.clone()]);
                    warn!(%repository, path, %error, "code-graph missing importer retire failed");
                }
            }
            Err(error) => debug!(%repository, path, %error, "unreadable importer; skipped"),
        }
    }
    if reparsed > 0 || retired > 0 || reconciled > 0 {
        info!(
            %repository,
            revision = %revision.0,
            reparsed,
            retired,
            reconciled,
            pending_importers = importers.paths.len(),
            "code-graph updated from the working tree"
        );
    }
    succeeded
}

/// The revision a fold of the **working tree** is stamped with: `<HEAD>+workdir`
/// when the tree carries uncommitted changes, the bare commit when it does not.
///
/// One function for one question, used by the incremental watcher and by the
/// full [`scan_repository`] alike. They were two — the watcher stamped
/// `+workdir` and the full scan stamped bare `HEAD` for the identical bytes — so
/// the same symbol flipped between the two forms depending only on which path
/// folded it last, and a full build of a dirty tree was immediately reported
/// stale by `graph status` (2026-08-13 review, codegraph F6).
///
/// A checkout with no resolvable `HEAD` already reports `"workdir"`, which is
/// left alone rather than doubled.
pub fn working_tree_revision(root: &Path) -> GitRevision {
    let head = head_revision(root);
    if head.0 == "workdir" || !working_tree_dirty(root) {
        head
    } else {
        GitRevision(format!("{}+workdir", head.0))
    }
}

/// Whether the working tree has changes Git can see (tracked modifications or
/// untracked, non-ignored files). Best-effort: an unanswerable Git reports
/// "clean", which only ever makes a staleness verdict more conservative.
///
/// Lives here, beside [`head_revision`], because the stamp a fold writes and the
/// staleness verdict `graph status` renders must read dirtiness the same way; a
/// second copy in the status handler is how the two would drift apart.
#[must_use]
pub fn working_tree_dirty(root: &Path) -> bool {
    git_command(root)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| !output.stdout.is_empty())
}

/// The repo-relative form of a watched path, matching exactly what the full scan
/// stores in `code_nodes.source_path`. `None` when the path is not under `root`
/// (a symlinked tree, or an event for a sibling directory).
fn repo_relative(root: &Path, path: &Path) -> Option<String> {
    if let Ok(stripped) = path.strip_prefix(root) {
        return Some(stripped.to_string_lossy().into_owned());
    }
    // notify reports the path the OS handed it, which can differ from the
    // canonical root by a symlinked ancestor (`/tmp` → `/private/tmp`).
    // Canonicalizing the PARENT recovers the match without resolving the file
    // itself — which would fail for the delete events that matter most.
    let canonical_parent = path.parent()?.canonicalize().ok()?;
    let name = path.file_name()?;
    let stripped = canonical_parent.strip_prefix(root).ok()?.join(name);
    Some(stripped.to_string_lossy().into_owned())
}

// --------------------------------------------------------------------------
// The `graph.*` query seam
// --------------------------------------------------------------------------

/// Backs the `graph.callers_of` / `graph.blast_radius` / `graph.tests_covering`
/// tools, and any CLI/TUI pane that asks the same questions.
///
/// The queries themselves live in `codypendent_knowledge::codegraph` (bounded,
/// rendered, and tested there); this is only the assembly seam — the pool the
/// graph lives in, plus [`repository_id_for`], so a caller that knows a
/// repository ROOT (which is all a run knows) resolves to the SAME identity the
/// scan wrote under. Deriving it any other way is how the TUI ended up querying
/// an id the daemon never stored under (2026-08-13 review, F5).
#[derive(Clone)]
pub struct PoolCodeGraph {
    pool: SqlitePool,
}

impl PoolCodeGraph {
    /// Bind the seam to the daemon's pool.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl codegraph::CodeGraphQueries for PoolCodeGraph {
    async fn ask(
        &self,
        repository_root: &Path,
        question: codegraph::GraphQuestion,
    ) -> Result<codegraph::GraphAnswer, String> {
        let repository = repository_id_for(repository_root);
        let mut answer = codegraph::answer(&self.pool, repository, &question)
            .await
            .map_err(|error| format!("code-graph query failed: {error}"))?;
        let ids: Vec<String> = answer
            .hits
            .iter()
            .take(codegraph::GRAPH_ANSWER_LIMIT)
            .map(|hit| hit.id.to_string())
            .collect();
        if let Ok(locations) = crate::codegraph_ops::locations::for_node_ids(
            &self.pool,
            repository,
            repository_root,
            &ids,
        )
        .await
        {
            for hit in &mut answer.hits {
                hit.location = locations.get(&hit.id.to_string()).cloned();
            }
        }
        Ok(answer)
    }
}

/// Canonicalize `root` (falling back to the path as-given) and derive the stable
/// [`RepositoryId`] the knowledge fabric attributes work under. Kept here so the
/// startup scan and the per-run executor derive identity identically.
#[must_use]
pub fn repository_id_for(root: &Path) -> RepositoryId {
    let canonical = discover_repository_root(root)
        .unwrap_or_else(|| root.canonicalize().unwrap_or_else(|_| PathBuf::from(root)));
    codypendent_knowledge::stable_repository_id(&canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn importer_refreshes_are_coalesced_and_drained_in_bounded_fifo_batches() {
        let paths: Vec<String> = (0..WATCH_BATCH_CAP + 7)
            .map(|index| format!("src/caller_{index}.py"))
            .collect();
        let mut queue = ImporterQueue::default();
        queue.extend(paths.clone());
        queue.extend(paths.clone());
        assert_eq!(queue.take_batch(), paths[..WATCH_BATCH_CAP]);
        queue.extend([paths[WATCH_BATCH_CAP].clone(), paths[0].clone()]);
        let mut remainder = paths[WATCH_BATCH_CAP..].to_vec();
        remainder.push(paths[0].clone());
        assert_eq!(queue.take_batch(), remainder);
        assert!(queue.is_empty());
    }

    fn init_repo(path: &Path) {
        let status = Command::new("git")
            .current_dir(path)
            .args(["init", "--quiet"])
            .status()
            .expect("run git init");
        assert!(status.success());
    }

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

    #[test]
    fn candidate_paths_are_judged_relative_to_the_checkout() {
        // The checkout's own ancestors are none of the filter's business. Judging
        // the ABSOLUTE path rejected every file in a repository living under a
        // dot-directory — `/tmp/.tmpXk9/…`, `~/.local/share/…`, a checkout inside
        // a dot-prefixed workspace — and the watcher then folded nothing at all.
        let root = Path::new("/tmp/.tmpXk9/repo");
        assert!(is_candidate_path(root, &root.join("src/lib.rs")));
        assert!(is_candidate_path(root, &root.join("crates/a/src/mod.rs")));

        // Inside the checkout the rules still bite.
        assert!(!is_candidate_path(
            root,
            &root.join("target/debug/build.rs")
        ));
        assert!(!is_candidate_path(root, &root.join(".git/hooks/x.rs")));
        assert!(!is_candidate_path(root, &root.join(".cargo/config.rs")));
        assert!(!is_candidate_path(root, &root.join("src/nested")));
        assert!(!is_candidate_path(root, &root.join("README.md")));
    }

    #[test]
    fn the_watcher_accepts_every_language_the_parser_handles() {
        // The watcher's filter and the parser's grammar table must be ONE list.
        // This test is that equality, written so it fails the day a language is
        // added to `language_for` and not to the watcher — which cannot happen
        // while `is_candidate_path` calls it, and is exactly what happened when
        // the two were maintained separately.
        let root = Path::new("/tmp/.tmpXk9/repo");
        for extension in codegraph::supported_extensions() {
            let path = root.join(format!("src/thing.{extension}"));
            assert!(
                is_candidate_path(root, &path),
                "the watcher rejects .{extension}, which the parser handles"
            );
        }
        // And a language nothing parses is still rejected, cheaply, before the
        // path even reaches the queue.
        for extension in ["md", "go", "java", "lock", "json"] {
            assert!(
                !is_candidate_path(root, &root.join(format!("src/thing.{extension}"))),
                ".{extension} must not reach the fold queue"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_live_edit_to_a_python_file_reaches_the_graph() {
        // Outcome 14 for the languages the user actually writes in. The filter
        // test above proves the gate; this drives the whole live path — notify
        // callback, debounce, `apply_batch`, `upsert_file_graph` — against an
        // uncommitted `.py` edit, which before the widening never reached the
        // channel at all.
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/api.py"), "def handler():\n    return 1\n").unwrap();
        init_repo(&root);

        let data = tempfile::tempdir().unwrap();
        let pool = codypendent_knowledge::db::open(&data.path().join("graph.db"))
            .await
            .unwrap();
        let repository = repository_id_for(&root);
        {
            let _guard = lock_repository(repository).await;
            scan_repository(&pool, repository, &root).await.unwrap();
        }
        assert!(
            codegraph::find_symbols(&pool, repository, "uncommitted_python_symbol", 5)
                .await
                .unwrap()
                .is_empty()
        );

        let _watcher = arm_watcher(pool.clone(), repository, &root).expect("arm the watcher");
        std::fs::write(
            root.join("src/api.py"),
            "def handler():\n    return 1\n\n\ndef uncommitted_python_symbol():\n    return handler()\n",
        )
        .unwrap();

        // Generous against WATCH_DEBOUNCE (400 ms) so a loaded box does not
        // flake; the assertion fails on timeout rather than passing silently.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let found = !codegraph::find_symbols(&pool, repository, "uncommitted_python_symbol", 5)
                .await
                .unwrap()
                .is_empty();
            if found {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the watcher never folded the edited Python file"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_watcher_refreshes_unchanged_importers_when_definitions_change() {
        async fn wait_for_call(
            pool: &SqlitePool,
            repository: RepositoryId,
            target_path: &str,
            target_kind: &str,
            previous: Option<&str>,
        ) -> String {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let target: Option<String> = sqlx::query_scalar(
                    "SELECT t.id FROM code_edges e \
                     JOIN code_nodes f ON f.id = e.from_node \
                     JOIN code_nodes t ON t.id = e.to_node \
                     WHERE f.repository = ? AND t.repository = ? \
                     AND f.source_path = 'src/main.py' AND f.qualified_name = 'caller' \
                     AND e.relation = 'calls' AND t.qualified_name = 'target' \
                     AND t.source_path = ? AND t.kind = ?",
                )
                .bind(repository.to_string())
                .bind(repository.to_string())
                .bind(target_path)
                .bind(target_kind)
                .fetch_optional(pool)
                .await
                .unwrap();
                if let Some(target) = target.filter(|target| previous != Some(target.as_str())) {
                    return target;
                }
                assert!(
                    Instant::now() < deadline,
                    "the unchanged caller never refreshed its {target_kind} target in {target_path}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        let caller = root.join("src/main.py");
        let caller_source = "from service import target\n\ndef caller():\n    return target(1)\n";
        std::fs::write(&caller, caller_source).unwrap();
        let modified = std::fs::metadata(&caller).unwrap().modified().unwrap();
        let callee = root.join("src/service.py");
        std::fs::write(&callee, "def placeholder():\n    return 1\n").unwrap();
        init_repo(&root);
        let data = tempfile::tempdir().unwrap();
        let pool = codypendent_knowledge::db::open(&data.path().join("graph.db"))
            .await
            .unwrap();
        let repository = repository_id_for(&root);
        scan_repository(&pool, repository, &root).await.unwrap();
        wait_for_call(
            &pool,
            repository,
            "src/main.py",
            "external_dependency",
            None,
        )
        .await;
        let _watcher = arm_watcher(pool.clone(), repository, &root).unwrap();

        std::fs::write(&callee, "def target(value):\n    return value\n").unwrap();
        let first = wait_for_call(&pool, repository, "src/service.py", "function", None).await;

        std::fs::write(
            &callee,
            "def target(value, extra=2):\n    return value + extra\n",
        )
        .unwrap();
        wait_for_call(
            &pool,
            repository,
            "src/service.py",
            "function",
            Some(&first),
        )
        .await;

        std::fs::remove_file(&callee).unwrap();
        wait_for_call(
            &pool,
            repository,
            "src/main.py",
            "external_dependency",
            None,
        )
        .await;

        std::fs::write(&callee, "def target(value):\n    return value\n").unwrap();
        wait_for_call(&pool, repository, "src/service.py", "function", None).await;
        assert_eq!(std::fs::read_to_string(&caller).unwrap(), caller_source);
        assert_eq!(
            std::fs::metadata(caller).unwrap().modified().unwrap(),
            modified
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_watcher_finishes_importer_fanout_without_further_events() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        let callers = WATCH_BATCH_CAP + 3;
        for index in 0..callers {
            std::fs::write(
                root.join(format!("src/caller_{index:03}.py")),
                "from service import target\n\ndef caller():\n    return target()\n",
            )
            .unwrap();
        }
        let callee = root.join("src/service.py");
        std::fs::write(&callee, "def placeholder():\n    return 1\n").unwrap();
        init_repo(&root);
        let data = tempfile::tempdir().unwrap();
        let pool = codypendent_knowledge::db::open(&data.path().join("graph.db"))
            .await
            .unwrap();
        let repository = repository_id_for(&root);
        scan_repository(&pool, repository, &root).await.unwrap();
        let _watcher = arm_watcher(pool.clone(), repository, &root).unwrap();
        std::fs::write(callee, "def target():\n    return 1\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let refreshed: i64 = sqlx::query_scalar(
                "SELECT COUNT(DISTINCT f.source_path) FROM code_edges e \
                 JOIN code_nodes f ON f.id = e.from_node \
                 JOIN code_nodes t ON t.id = e.to_node \
                 WHERE f.repository = ? AND t.repository = ? AND e.relation = 'calls' \
                 AND f.qualified_name = 'caller' AND t.source_path = 'src/service.py' \
                 AND t.qualified_name = 'target' AND t.kind = 'function'",
            )
            .bind(repository.to_string())
            .bind(repository.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
            if refreshed == callers as i64 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "only {refreshed}/{callers} queued importers refreshed"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn the_watcher_captures_dependents_of_a_queued_importer_edited_before_its_event() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/service.py"), "def target():\n    return 1\n").unwrap();
        std::fs::write(
            root.join("src/middle.py"),
            "from service import target\n\ndef middle(value):\n    return target()\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/outer.py"),
            "from middle import middle\n\ndef outer():\n    return middle(1)\n",
        )
        .unwrap();
        init_repo(&root);
        let data = tempfile::tempdir().unwrap();
        let pool = codypendent_knowledge::db::open(&data.path().join("graph.db"))
            .await
            .unwrap();
        let repository = repository_id_for(&root);
        scan_repository(&pool, repository, &root).await.unwrap();
        let mut importers = ImporterQueue::default();
        importers.extend(["src/middle.py".to_owned()]);
        std::fs::write(
            root.join("src/middle.py"),
            "from service import target\n\ndef middle(value, extra=2):\n    return target()\n",
        )
        .unwrap();
        let mut folded = HashMap::new();
        let pending = HashSet::new();
        assert!(
            apply_batch(
                &pool,
                repository,
                &root,
                &pending,
                &mut folded,
                &mut importers
            )
            .await
        );
        assert!(
            !importers.is_empty(),
            "the queued edit must capture its caller"
        );
        assert!(
            apply_batch(
                &pool,
                repository,
                &root,
                &pending,
                &mut folded,
                &mut importers
            )
            .await
        );
        assert!(importers.is_empty());
        let calls: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM code_edges e JOIN code_nodes f ON f.id = e.from_node \
             JOIN code_nodes t ON t.id = e.to_node WHERE f.repository = ? AND t.repository = ? \
             AND f.source_path = 'src/outer.py' AND f.qualified_name = 'outer' \
             AND t.source_path = 'src/middle.py' AND t.qualified_name = 'middle' \
             AND t.kind = 'function' AND e.relation = 'calls'",
        )
        .bind(repository.to_string())
        .bind(repository.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(calls, 1);
    }

    #[test]
    fn a_new_top_level_directory_is_the_one_non_source_path_the_filter_admits() {
        // The gate for F8, as a predicate. A directory carries no source
        // extension, so `is_candidate_path` discards it — and the debounce loop
        // is the only caller of `arm_source_subtrees`, so discarding it left a
        // package added mid-session outside every watch.
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        for name in ["newpkg", "target", ".hidden"] {
            std::fs::create_dir_all(root.join(name).join("src")).unwrap();
        }
        std::fs::write(root.join("Cargo.toml"), "").unwrap();

        assert!(is_new_top_level_directory(&root, &root.join("newpkg")));
        // Everything else is still rejected before the bounded queue: the filter
        // exists so a build's event storm cannot fill it.
        assert!(!is_new_top_level_directory(&root, &root.join("target")));
        assert!(!is_new_top_level_directory(&root, &root.join(".hidden")));
        assert!(!is_new_top_level_directory(&root, &root.join("Cargo.toml")));
        assert!(!is_new_top_level_directory(&root, &root.join("missing")));
        // Deeper directories are already inside a recursive watch, so they need
        // no re-arming — and keeping them out is what keeps the stat off the
        // hot path.
        assert!(!is_new_top_level_directory(&root, &root.join("newpkg/src")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_top_level_package_added_mid_session_folds_with_no_further_edits() {
        // F8 end to end. Before this, the directory-creation event was dropped
        // in the notify callback, so no batch started, so `arm_source_subtrees`
        // never ran — and the new package's symbols needed TWO further unrelated
        // edits before they appeared (2026-08-13 review, codegraph F8).
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn existing() {}\n").unwrap();
        init_repo(&root);

        let data = tempfile::tempdir().unwrap();
        let pool = codypendent_knowledge::db::open(&data.path().join("graph.db"))
            .await
            .unwrap();
        let repository = repository_id_for(&root);
        {
            let _guard = lock_repository(repository).await;
            scan_repository(&pool, repository, &root).await.unwrap();
        }
        let _watcher = arm_watcher(pool.clone(), repository, &root).expect("arm the watcher");

        // The exact move the finding describes: a brand-new top-level package,
        // written in one go, and then nothing else touched at all.
        std::fs::create_dir_all(root.join("newpkg/src")).unwrap();
        std::fs::write(
            root.join("newpkg/src/lib.rs"),
            "pub fn brand_new_top_level_symbol() -> u32 { 42 }\n",
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let found =
                !codegraph::find_symbols(&pool, repository, "brand_new_top_level_symbol", 5)
                    .await
                    .unwrap()
                    .is_empty();
            if found {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "a new top-level package never reached the graph"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[test]
    fn the_walk_offers_every_language_the_parser_handles_and_counts_the_rest() {
        // The full-scan gate, the other half of the same class. Before this it
        // was `ext == "rs"`, so a Python/TypeScript repository walked its whole
        // tree and offered the parser nothing.
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        for extension in codegraph::supported_extensions() {
            std::fs::write(root.join(format!("src/thing.{extension}")), "").unwrap();
        }
        std::fs::write(root.join("src/notes.md"), "").unwrap();
        std::fs::write(root.join("go.mod"), "").unwrap();
        std::fs::write(root.join("src/main.go"), "").unwrap();

        let (paths, summary) = collect_source_paths(root, SCAN_FILE_CAP).unwrap();
        let found: HashSet<String> = paths.iter().map(|(path, _)| path.clone()).collect();
        for extension in codegraph::supported_extensions() {
            let expected = format!("src/thing.{extension}");
            assert!(
                found.contains(&expected),
                "the walk skipped {expected}; found {found:?}"
            );
        }
        assert_eq!(
            summary.files_supported,
            codegraph::supported_extensions().len()
        );
        // Everything else is COUNTED, not silently dropped: that count is the
        // only thing that can explain an empty graph to a user.
        assert_eq!(summary.files_skipped_unsupported, 3);
        assert_eq!(summary.unsupported_by_extension.get("go"), Some(&1));
        assert_eq!(summary.unsupported_by_extension.get("mod"), Some(&1));
        assert_eq!(summary.unsupported_by_extension.get("md"), Some(&1));
        assert!(!summary.truncated_by_cap);
    }

    #[test]
    fn a_truncated_walk_says_so() {
        // `SCAN_FILE_CAP` used to truncate in silence, so a repository larger
        // than the cap got a partial graph presented as a complete one.
        let repo = tempfile::tempdir().unwrap();
        for index in 0..6 {
            std::fs::write(repo.path().join(format!("f{index}.rs")), "").unwrap();
        }
        let (paths, summary) = collect_source_paths(repo.path(), 3).unwrap();
        assert_eq!(paths.len(), 3);
        assert!(summary.truncated_by_cap);
        assert_eq!(summary.file_cap, 3);
        assert!(
            summary.headline().contains("TRUNCATED"),
            "{}",
            summary.headline()
        );
    }

    /// A checkout shaped like the reported one: an ignored dependency tree whose
    /// name sorts before the application's, holding `count` `.js` files, and a
    /// three-file `src/`. `ignored_dir` is excluded by `.gitignore` alone — the
    /// name guard must not be what saves this test.
    ///
    /// The dependency files sit at the SAME depth as `src/`'s, so what this
    /// measures is the ignore filter and not the traversal order: a walk that
    /// merely visited the tree in a different sequence would still spend its
    /// whole budget here, which is the property under test.
    fn seed_dependency_heavy_repository(root: &Path, ignored_dir: &str, count: usize) {
        init_repo(root);
        std::fs::write(root.join(".gitignore"), format!("{ignored_dir}/\n")).unwrap();
        let deps = root.join(ignored_dir);
        std::fs::create_dir_all(&deps).unwrap();
        for index in 0..count {
            std::fs::write(
                deps.join(format!("chunk{index:05}.js")),
                format!("export function vendored{index}() {{ return {index}; }}\n"),
            )
            .unwrap();
        }
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/app.js"),
            "export function renderApp() { return boot(); }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/boot.ts"),
            "export function boot(): number { return 1; }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn native_helper() -> u32 { 1 }\n",
        )
        .unwrap();
    }

    #[test]
    fn an_ignored_tree_cannot_spend_the_file_cap() {
        // The reported defect, as a unit. The ignore filter used to run AFTER the
        // walk had already spent its budget, so a `node_modules`-shaped tree —
        // sorting before `src/`, and full of files the new JavaScript and
        // TypeScript grammars now recognise — consumed the cap and the
        // application was never reached at all.
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        // `deps` is ignored by the checkout's own rules and by nothing else, so
        // this measures the ignore rules, not the dependency-name guard.
        seed_dependency_heavy_repository(&root, "deps", 40);

        let (paths, summary) = collect_source_paths(&root, 6).unwrap();
        let found: Vec<&str> = paths.iter().map(|(path, _)| path.as_str()).collect();
        println!("collected: {found:?}");
        println!("summary: {}", summary.headline());

        assert_eq!(found, ["src/app.js", "src/boot.ts", "src/lib.rs"]);
        assert!(
            !summary.truncated_by_cap,
            "40 ignored files exhausted a 6-file cap: {}",
            summary.headline()
        );
        // The walk never went in, so those files are in no count here — which is
        // exactly what `dirs_pruned` exists to say out loud.
        assert_eq!(summary.files_seen, 3);
        assert_eq!(summary.files_skipped_ignored, 0);
        assert!(summary.dirs_pruned >= 1, "{}", summary.headline());
        assert!(
            summary.headline().contains("director(ies) not walked"),
            "{}",
            summary.headline()
        );
    }

    #[test]
    fn an_individually_ignored_file_is_counted_not_pruned() {
        // The other half of the same filter: a rule that names FILES still has to
        // report them, or `graph build` cannot explain a smaller graph.
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        init_repo(&root);
        std::fs::write(root.join(".gitignore"), "*.min.js\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/app.js"), "export function a() {}\n").unwrap();
        std::fs::write(root.join("src/bundle.min.js"), "export function b() {}\n").unwrap();

        let (paths, summary) = collect_source_paths(&root, SCAN_FILE_CAP).unwrap();
        assert_eq!(
            paths.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            ["src/app.js"]
        );
        assert_eq!(summary.files_seen, 2);
        assert_eq!(summary.files_supported, 2);
        assert_eq!(summary.files_skipped_ignored, 1);
        assert!(
            summary.headline().contains("1 ignored"),
            "{}",
            summary.headline()
        );
    }

    #[test]
    fn a_dependency_tree_is_pruned_even_when_git_cannot_answer() {
        // `ignored_paths` is best-effort: no Git, a broken repository, any spawn
        // failure and it excludes NOTHING. The name guard is the floor under that
        // — without it the degraded case is the original defect verbatim, since a
        // vendored `node_modules` is exactly the tree that ate the cap.
        let plain = tempfile::tempdir().unwrap();
        let root = plain.path().canonicalize().unwrap();
        // Deliberately NOT a git repository: `git check-ignore` cannot answer.
        std::fs::create_dir_all(root.join("node_modules/react")).unwrap();
        for index in 0..40 {
            std::fs::write(
                root.join(format!("node_modules/react/chunk{index:05}.js")),
                "export function vendored() {}\n",
            )
            .unwrap();
        }
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/app.js"), "export function renderApp() {}\n").unwrap();

        let (paths, summary) = collect_source_paths(&root, 6).unwrap();
        assert!(ignored_paths(&root, &["node_modules".to_string()]).is_empty());
        assert_eq!(
            paths.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            ["src/app.js"]
        );
        assert!(!summary.truncated_by_cap, "{}", summary.headline());
    }

    #[tokio::test]
    async fn a_second_build_does_not_replace_a_good_graph_with_an_empty_one() {
        // The whole finding, end to end through the production scan, at the real
        // `SCAN_FILE_CAP`: build once to establish a graph, build again, and the
        // second build must not retire the repository. Before the fix the ignored
        // tree spent the cap, the walk never reached `src/`, and
        // `rebuild_repository` retired every path it had not seen — an explicit
        // `graph build` wiping a valid graph and reporting a near-empty one.
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        seed_dependency_heavy_repository(&root, "deps", SCAN_FILE_CAP + 1);

        let data = tempfile::tempdir().unwrap();
        let pool = codypendent_knowledge::db::open(&data.path().join("graph.db"))
            .await
            .unwrap();
        let repository = repository_id_for(&root);
        let _guard = lock_repository(repository).await;

        let first = scan_repository(&pool, repository, &root).await.unwrap();
        let nodes_after_first: i64 =
            sqlx::query_scalar("SELECT count(*) FROM code_nodes WHERE repository = ?")
                .bind(repository.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        println!("first: {} → {nodes_after_first} nodes", first.headline());
        assert_eq!(first.files_folded, 3, "{}", first.headline());
        assert!(nodes_after_first > 3);

        let second = scan_repository(&pool, repository, &root).await.unwrap();
        let nodes_after_second: i64 =
            sqlx::query_scalar("SELECT count(*) FROM code_nodes WHERE repository = ?")
                .bind(repository.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        println!("second: {} → {nodes_after_second} nodes", second.headline());

        assert_eq!(second.files_folded, 3, "{}", second.headline());
        assert_eq!(
            second.retired,
            codegraph::RetiredFiles::default(),
            "the second build retired live files: {}",
            second.headline()
        );
        assert_eq!(
            nodes_after_second, nodes_after_first,
            "a second `graph build` changed the graph it should have reproduced"
        );
        let paths: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT source_path FROM code_nodes WHERE repository = ? ORDER BY 1",
        )
        .bind(repository.to_string())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(paths, ["src/app.js", "src/boot.ts", "src/lib.rs"]);
    }

    #[test]
    fn the_ignore_rules_file_is_the_only_non_source_file_the_watcher_admits() {
        // Fix 2's gate, as a predicate. A `.gitignore` edit re-scopes files that
        // emit no event of their own, so its own event has to reach the
        // debouncer — and nothing else may ride in with it, because the queue it
        // feeds is bounded and a full queue escalates the whole batch.
        let root = Path::new("/tmp/.tmpXk9/repo");
        assert!(is_ignore_rules_file(root, &root.join(".gitignore")));
        assert!(is_ignore_rules_file(
            root,
            &root.join("crates/a/.gitignore")
        ));

        // Not this repository's rules, and not rules at all.
        assert!(!is_ignore_rules_file(root, &root.join(".git/info/exclude")));
        assert!(!is_ignore_rules_file(
            root,
            &root.join(".git/modules/x/.gitignore")
        ));
        assert!(!is_ignore_rules_file(
            root,
            &root.join("target/debug/.gitignore")
        ));
        assert!(!is_ignore_rules_file(root, &root.join(".gitattributes")));
        assert!(!is_ignore_rules_file(root, &root.join(".env")));
        assert!(!is_ignore_rules_file(root, &root.join("src/lib.rs")));
        assert!(!is_ignore_rules_file(root, &root.join("gitignore")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_gitignore_edit_removes_the_newly_excluded_file_from_the_graph() {
        // Fix 2 end to end. A `.gitignore` edit is neither a supported source
        // path nor a new top-level directory, so its event was discarded: a file
        // that had just become ignored stayed in the graph indefinitely, and a
        // later source event could not repair it either, because `apply_batch`
        // SKIPS an ignored path rather than retiring what is stored for it. So
        // `graph show` kept serving excluded code until someone ran a manual
        // build.
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn kept_symbol() -> u32 { 1 }\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("generated")).unwrap();
        std::fs::write(
            root.join("generated/schema.rs"),
            "pub fn folded_then_excluded() -> u32 { 2 }\n",
        )
        .unwrap();
        init_repo(&root);

        let data = tempfile::tempdir().unwrap();
        let pool = codypendent_knowledge::db::open(&data.path().join("graph.db"))
            .await
            .unwrap();
        let repository = repository_id_for(&root);
        {
            let _guard = lock_repository(repository).await;
            scan_repository(&pool, repository, &root).await.unwrap();
        }
        assert!(
            !codegraph::find_symbols(&pool, repository, "folded_then_excluded", 5)
                .await
                .unwrap()
                .is_empty(),
            "the generated file must be in the graph before it is excluded"
        );

        let _watcher = arm_watcher(pool.clone(), repository, &root).expect("arm the watcher");
        // The whole move: edit the ignore rules, touch nothing else.
        std::fs::write(root.join(".gitignore"), "generated/\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let still_there =
                !codegraph::find_symbols(&pool, repository, "folded_then_excluded", 5)
                    .await
                    .unwrap()
                    .is_empty();
            if !still_there {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "a `.gitignore` edit never removed the excluded file from the graph"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // …and the rescan it scheduled kept everything the rules still allow.
        assert!(
            !codegraph::find_symbols(&pool, repository, "kept_symbol", 5)
                .await
                .unwrap()
                .is_empty(),
            "the rescan dropped a file the rules still allow"
        );
    }

    #[tokio::test]
    async fn a_mixed_language_repository_folds_every_language_not_only_rust() {
        // The user-visible bug, end to end through the production scan: a repo
        // with a Python file, a TSX file and a Rust file produced ONE row —
        // `('src/lib.rs', 2)` — and an empty graph plus silence for a project
        // with no Rust in it at all.
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        seed_mixed_repository(&root);

        let data = tempfile::tempdir().unwrap();
        let pool = codypendent_knowledge::db::open(&data.path().join("graph.db"))
            .await
            .unwrap();
        let repository = repository_id_for(&root);
        let _guard = lock_repository(repository).await;
        let summary = scan_repository(&pool, repository, &root).await.unwrap();

        // Grouped exactly the way the live check reads the database back.
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT source_path, language, count(*) FROM code_nodes \
             GROUP BY source_path, language ORDER BY source_path",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        println!("code_nodes by source_path: {rows:#?}");
        println!("summary: {}", summary.headline());

        let paths: Vec<&str> = rows.iter().map(|(path, _, _)| path.as_str()).collect();
        assert_eq!(paths, ["src/app.tsx", "src/lib.rs", "src/main.py"]);
        for (path, language, count) in &rows {
            assert!(*count > 1, "{path} folded to {count} node(s) — file only");
            let expected = match path.as_str() {
                "src/app.tsx" => "tsx",
                "src/lib.rs" => "rust",
                _ => "python",
            };
            assert_eq!(language, expected, "{path}");
        }

        assert_eq!(summary.files_folded, 3);
        assert_eq!(summary.folded_by_language.get("python"), Some(&1));
        assert_eq!(summary.folded_by_language.get("tsx"), Some(&1));
        assert_eq!(summary.folded_by_language.get("rust"), Some(&1));
        assert!(!summary.found_nothing_to_fold());

        // Every language contributes real relations, not just a File node.
        let calls: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM code_edges e JOIN code_nodes n ON e.from_node = n.id \
             WHERE e.relation = 'calls' AND n.source_path = ?",
        )
        .bind("src/main.py")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(calls > 0, "python produced no Calls edges");
    }

    #[tokio::test]
    async fn a_repository_with_no_parsable_source_reports_why() {
        // The silence is as much the bug as the missing grammars: an all-Go
        // repository must come back saying "1 file seen, none parsable", not
        // "scan complete".
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        std::fs::write(root.join("main.go"), "package main\n").unwrap();
        std::fs::write(root.join("README.md"), "# hi\n").unwrap();
        init_repo(&root);

        let data = tempfile::tempdir().unwrap();
        let pool = codypendent_knowledge::db::open(&data.path().join("graph.db"))
            .await
            .unwrap();
        let repository = repository_id_for(&root);
        let _guard = lock_repository(repository).await;
        let summary = scan_repository(&pool, repository, &root).await.unwrap();

        assert!(summary.found_nothing_to_fold());
        assert_eq!(summary.files_folded, 0);
        assert_eq!(summary.files_seen, 2);
        assert_eq!(summary.files_skipped_unsupported, 2);
        let headline = summary.headline();
        println!("headline: {headline}");
        assert!(headline.contains("NO supported source found"), "{headline}");
        assert!(headline.contains(".go 1"), "{headline}");
    }

    /// A committed repository holding one Python, one TSX and one Rust file,
    /// each defining a function that calls another.
    fn seed_mixed_repository(root: &Path) {
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/main.py"),
            "from .service import dispatch\n\n\n\
             def handler(request):\n    return route(request)\n\n\n\
             def route(request):\n    return dispatch(request)\n\n\n\
             class Router:\n    def decide(self, request):\n        return handler(request)\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/app.tsx"),
            "import { useState } from \"react\";\n\n\
             export function greet(name: string): string {\n  return format(name);\n}\n\n\
             function format(name: string): string {\n  return `hi ${name}`;\n}\n\n\
             export interface Props { name: string }\n\n\
             export const App = (props: Props) => {\n\
             \x20 const [n] = useState(props.name);\n  return <div>{greet(n)}</div>;\n};\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn boot() -> u32 {\n    classify()\n}\n\npub fn classify() -> u32 { 1 }\n",
        )
        .unwrap();
        init_repo(root);
    }

    #[test]
    fn ignored_paths_asks_git_and_degrades_to_an_empty_set() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        std::fs::write(repo.path().join(".gitignore"), "generated/\n").unwrap();
        std::fs::create_dir_all(repo.path().join("generated")).unwrap();
        std::fs::write(repo.path().join("generated/a.rs"), "").unwrap();
        std::fs::write(repo.path().join("kept.rs"), "").unwrap();

        let probe = vec!["generated/a.rs".to_string(), "kept.rs".to_string()];
        let ignored = ignored_paths(repo.path(), &probe);
        assert!(ignored.contains("generated/a.rs"), "{ignored:?}");
        assert!(!ignored.contains("kept.rs"), "{ignored:?}");

        // Outside a checkout Git cannot answer; the filter must then exclude
        // nothing rather than excluding everything.
        let plain = tempfile::tempdir().unwrap();
        assert!(ignored_paths(plain.path(), &probe).is_empty());
        assert!(ignored_paths(repo.path(), &[]).is_empty());
    }

    #[test]
    fn a_working_tree_fold_is_stamped_so_the_revision_says_uncommitted() {
        let repo = tempfile::tempdir().unwrap();
        // No commits yet: `head_revision` reports the `workdir` placeholder, which
        // must not be doubled into `workdir+workdir`.
        assert_eq!(working_tree_revision(repo.path()).0, "workdir");
    }

    #[test]
    fn repository_discovery_rejects_an_ordinary_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(discover_repository_root(dir.path()), None);
    }

    // ----------------------------------------------------------------------
    // The home-directory graph: a symbol budget, cache-tree exclusion, and a
    // root that fails closed.
    // ----------------------------------------------------------------------

    /// A Rust file defining `count` free functions — the shape of a generated
    /// FFI binding module, which is what the measured 10,218-node file was.
    fn generated_bindings(count: usize) -> String {
        let mut source = String::new();
        for index in 0..count {
            source.push_str(&format!(
                "pub fn generated_binding_{index}() -> u32 {{ {index} }}\n"
            ));
        }
        source
    }

    #[tokio::test]
    async fn one_pathological_file_cannot_spend_the_graph_on_its_own() {
        // The reported defect, as a unit. `SCAN_FILE_CAP` counts FILES, and the
        // worst single file in the measured database folded to 10,218 nodes on
        // its own — so 2000 files of that shape produced 386,572 nodes while
        // never once tripping a 2000-FILE cap. A per-file symbol cap is the only
        // thing that sees it coming.
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn real_symbol() -> u32 { 1 }\n",
        )
        .unwrap();
        // Comfortably over `SCAN_FILE_NODE_CAP`, and nothing else in the tree is.
        std::fs::write(
            root.join("src/bindings.rs"),
            generated_bindings(SCAN_FILE_NODE_CAP + 500),
        )
        .unwrap();
        init_repo(&root);

        let data = tempfile::tempdir().unwrap();
        let pool = codypendent_knowledge::db::open(&data.path().join("graph.db"))
            .await
            .unwrap();
        let repository = repository_id_for(&root);
        let _guard = lock_repository(repository).await;
        let summary = scan_repository(&pool, repository, &root).await.unwrap();
        println!("summary: {}", summary.headline());

        assert_eq!(summary.files_skipped_oversized, 1, "{}", summary.headline());
        // The rest of the repository is still folded — an oversized file is
        // skipped, not a reason to abandon the scan.
        assert_eq!(summary.files_folded, 1, "{}", summary.headline());
        assert!(summary.nodes < SCAN_FILE_NODE_CAP, "{}", summary.headline());

        let paths: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT source_path FROM code_nodes WHERE repository = ? ORDER BY 1",
        )
        .bind(repository.to_string())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            paths,
            ["src/lib.rs"],
            "the generated module reached the graph"
        );

        // The same discipline `SCAN_FILE_CAP` already had: a scan that declined
        // to fold a path has not established the path is gone, so it retires
        // nothing.
        assert!(summary.truncated_by_cap, "{}", summary.headline());
        assert_eq!(summary.retired, codegraph::RetiredFiles::default());
        // …and it names the offender, so the next person reads WHY instead of
        // inferring it from a 1.9 GB database.
        let headline = summary.headline();
        assert!(headline.contains("src/bindings.rs"), "{headline}");
        assert!(headline.contains("per-file cap"), "{headline}");
    }

    #[test]
    fn the_node_budget_stops_the_fold_and_keeps_what_it_accepted() {
        // The whole-scan half. Driven through `apply_node_budget` directly with
        // small caps rather than a 120,000-symbol fixture — the constants are
        // parameters for exactly this.
        let repository = RepositoryId::new();
        let source = generated_bindings(10);
        // Measured, not assumed: the budget is set to exactly three files' worth
        // of whatever this source folds to, so the test cannot drift if the
        // parser's node set changes.
        let per_file = codegraph::measure_file_graph(repository, "src/f0.rs", &source)
            .unwrap()
            .nodes;
        assert!(per_file > 1, "a file folds to more than its File node");
        let budget = per_file * 3;
        let files: Vec<(String, String, Language)> = (0..6)
            .map(|index| (format!("src/f{index}.rs"), source.clone(), Language::Rust))
            .collect();
        let mut summary = ScanSummary::default();
        let kept = apply_node_budget(repository, files, &mut summary, 1_000, budget).unwrap();

        assert_eq!(kept.len(), 3, "{}", summary.headline());
        assert!(summary.truncated_by_node_budget, "{}", summary.headline());
        assert!(
            summary.truncated_by_cap,
            "a node-budget stop must suppress the retire pass: {}",
            summary.headline()
        );
        assert_eq!(summary.node_budget, budget);
        assert!(
            summary
                .headline()
                .contains(&format!("TRUNCATED at the {budget}-node budget")),
            "{}",
            summary.headline()
        );
        // The diagnostic names files it actually weighed, and no others.
        assert!(!summary.heaviest_files.is_empty());
        assert!(summary.heaviest_files.len() <= ScanSummary::MAX_HEAVIEST_FILES);
    }

    #[tokio::test]
    async fn the_node_budget_admits_this_workspace_whole() {
        // The number that justifies `SCAN_NODE_BUDGET` and `SCAN_FILE_NODE_CAP`,
        // measured rather than asserted: this workspace is a large real Rust
        // repository, and neither cap may bite on it. It prints its own numbers,
        // so the next person adjusting a cap has the measurement in front of
        // them instead of a guess.
        //
        // Skipped when the test is not run from inside the checkout (a packaged
        // crate), because then there is nothing to measure.
        let Some(root) = discover_repository_root(Path::new(env!("CARGO_MANIFEST_DIR"))) else {
            eprintln!("not in a checkout; nothing to measure");
            return;
        };
        let (files, mut summary) = collect_sources(&root, SCAN_FILE_CAP).unwrap();
        let repository = RepositoryId::new();
        let kept = apply_node_budget(
            repository,
            files,
            &mut summary,
            SCAN_FILE_NODE_CAP,
            SCAN_NODE_BUDGET,
        )
        .unwrap();
        let total: usize = summary.heaviest_files.first().map_or(0, |file| file.nodes);
        println!(
            "workspace: {} files kept; heaviest {} nodes; heaviest files {:?}",
            kept.len(),
            total,
            summary.heaviest_files
        );
        assert_eq!(
            summary.files_skipped_oversized, 0,
            "the per-file cap bit on hand-written source: {:?}",
            summary.heaviest_files
        );
        assert!(
            !summary.truncated_by_node_budget,
            "the node budget bit on this workspace"
        );
    }

    #[test]
    fn an_ide_cache_tree_is_never_walked() {
        // The tree that actually produced the graph: JetBrains' vendored Rust
        // stdlib under `Library/Caches/…` and Cursor's per-edit file history
        // under `Library/Application Support/…`. Neither is `.gitignore`d —
        // outside a checkout there are no rules to consult — so only a name
        // guard can stop them, exactly as `node_modules` is stopped.
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        init_repo(&root);
        for cache in [
            "Library/Caches/JetBrains/RustRover2026.1/intellij-rust/stdlib/vendor/windows-sys/src",
            "Library/Application Support/Cursor/User/History/1a2b",
            "Caches/pip/http/a/b",
            "AppData/Local/JetBrains/vendor/windows-sys",
            ".cache/uv/archive/pkg",
        ] {
            std::fs::create_dir_all(root.join(cache)).unwrap();
            std::fs::write(root.join(cache).join("mod.rs"), generated_bindings(50)).unwrap();
        }
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn real_symbol() {}\n").unwrap();
        // `vendor/` is NOT blanket-excluded: it is committed source in real
        // checkouts, and the vendored tree that caused the incident is already
        // gone with the cache root above it.
        std::fs::create_dir_all(root.join("vendor/ruff_python_ast")).unwrap();
        std::fs::write(
            root.join("vendor/ruff_python_ast/lib.rs"),
            "pub fn vendored_but_real() {}\n",
        )
        .unwrap();

        let (paths, summary) = collect_source_paths(&root, SCAN_FILE_CAP).unwrap();
        let found: Vec<&str> = paths.iter().map(|(path, _)| path.as_str()).collect();
        println!("collected: {found:?} / {}", summary.headline());
        assert_eq!(found, ["src/lib.rs", "vendor/ruff_python_ast/lib.rs"]);
        assert!(summary.dirs_pruned >= 5, "{}", summary.headline());
        // Pruned, not merely filtered: the walk never entered, so nothing inside
        // is counted anywhere.
        assert_eq!(summary.files_seen, 2, "{}", summary.headline());

        // And the watcher agrees, which it must: a path only one of them accepts
        // is folded by a batch and retired by the very next full scan, forever.
        assert!(!is_candidate_path(
            &root,
            &root.join("Library/Caches/JetBrains/mod.rs")
        ));
        assert!(is_candidate_path(
            &root,
            &root.join("vendor/ruff_python_ast/lib.rs")
        ));
    }

    #[test]
    fn the_home_directory_and_a_filesystem_root_are_refused() {
        // Belt and braces over `discover_repository_root`. On the machine that
        // produced the 510,904-node graph `$HOME` held no `.git`, so gate 1
        // should have refused it — a stray `git init ~` (or the `GIT_DIR` route
        // below) reopens the hole, and this is what closes it whatever answer
        // Git gives.
        assert!(is_implausible_scan_root(Path::new("/")));
        assert!(is_implausible_scan_root(Path::new("/Users")));
        assert!(is_implausible_scan_root(Path::new("/home")));
        assert!(is_implausible_scan_root(Path::new("/root")));

        let home = home_directory().expect("a home directory in the test environment");
        assert!(
            is_implausible_scan_root(&home.canonicalize().unwrap_or(home.clone())),
            "{} was accepted as a scan root",
            home.display()
        );
        // Refused whichever way Git answers for it.
        assert!(resolve_scan_root(&home).is_err());
        assert!(resolve_scan_root(Path::new("/")).is_err());
        assert_eq!(discover_repository_root(Path::new("/")), None);

        // An ordinary checkout is not caught by any of this.
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        init_repo(&root);
        assert!(!is_implausible_scan_root(&root));
        assert_eq!(resolve_scan_root(&root).ok(), Some(root));
    }

    #[tokio::test]
    async fn a_scan_of_the_home_directory_is_refused_loudly() {
        let data = tempfile::tempdir().unwrap();
        let pool = codypendent_knowledge::db::open(&data.path().join("graph.db"))
            .await
            .unwrap();
        let home = home_directory().expect("a home directory in the test environment");
        let error = scan_repository(&pool, RepositoryId::new(), &home)
            .await
            .expect_err("scanning the home directory must be refused");
        println!("refusal: {error}");
        // Refused, and refused in a way that says which of the two problems it
        // is — not a bare "not a git repository" for a directory that plainly is
        // the user's whole account.
        assert!(
            error.to_string().contains("refus") || error.to_string().contains("not a git"),
            "{error}"
        );
    }

    #[test]
    fn an_inherited_git_dir_cannot_forge_a_repository_root() {
        // THE ROUTE. `git rev-parse --show-toplevel` does not ask "is this a
        // repository" when `GIT_DIR` is set — it answers with the CURRENT
        // DIRECTORY. A daemon that inherited `GIT_DIR` (a git hook, `git rebase
        // -x`, `git bisect run`, an exported shell variable) therefore makes
        // `discover_repository_root` return `Some($HOME)` for a `$HOME` holding
        // no `.git` at all, and the gate in `scan_repository` waves it through.
        let real = tempfile::tempdir().unwrap();
        let real_root = real.path().canonicalize().unwrap();
        init_repo(&real_root);
        let plain = tempfile::tempdir().unwrap();
        let plain_root = plain.path().canonicalize().unwrap();

        // Part 1: the hazard is real in this Git, measured rather than assumed.
        let forged = Command::new("git")
            .current_dir(&plain_root)
            .env("GIT_DIR", real_root.join(".git"))
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .expect("run git");
        let forged_root = String::from_utf8_lossy(&forged.stdout).trim().to_string();
        println!("with GIT_DIR set, git names: {forged_root:?}");
        assert!(
            forged.status.success() && !forged_root.is_empty(),
            "this Git does not exhibit the GIT_DIR behaviour; the guard below is \
             still correct but this half of the test no longer measures anything"
        );

        // Part 2: the probe the scan uses removes the whole family from the
        // child's environment, so an inherited value cannot reach Git at all.
        let command = git_command(&plain_root);
        let removed: Vec<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .filter_map(|(key, _)| key.to_str().map(str::to_owned))
            .collect();
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_CEILING_DIRECTORIES",
        ] {
            assert!(
                removed.iter().any(|seen| seen == key),
                "{key} is still inherited by the repository probe: {removed:?}"
            );
        }

        // Part 3: with a clean environment, an ordinary directory is refused —
        // which is what the whole gate was always supposed to do.
        assert_eq!(discover_repository_root(&plain_root), None);
    }

    #[test]
    fn repository_identity_normalizes_subdirectories_to_the_checkout_root() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let nested = repo.path().join("crates").join("demo");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            discover_repository_root(&nested),
            repo.path().canonicalize().ok()
        );
        assert_eq!(repository_id_for(&nested), repository_id_for(repo.path()));
    }
}
