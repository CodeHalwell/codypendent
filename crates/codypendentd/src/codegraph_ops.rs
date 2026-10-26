//! Daemon-side handlers for `codypendent graph {build,status,show}` — the
//! assembly half of [`codypendent_daemon::codegraph`].
//!
//! # The bug this module closes
//!
//! The code graph was folded only as a side effect of opening a session or
//! starting a run. No command built it, no command described it, and
//! `index rebuild` — whose name reads as "build the index, graph included" —
//! explicitly does not touch it. On a mixed repository the graph came out
//! holding two nodes from one file while thousands of Python and TSX files
//! contributed nothing, and every surface reported that as a plain zero.
//!
//! So the *report* here is the feature, not the fold. [`CodeGraphOps::build`]
//! answers "why is my graph this size?" from two measured sources and no third
//! opinion:
//!
//! * the scan's own [`ScanSummary`] — files seen, files a grammar covered,
//!   files folded, the histogram of extensions no grammar covers, and whether
//!   the file cap truncated the walk. These are the extractor's tallies from
//!   the walk being reported, not a re-derivation of it;
//! * the graph tables themselves, for what the repository now *holds*
//!   (nodes/edges, and the per-language split with edge counts the summary does
//!   not carry).
//!
//! It would have been easy to re-walk the tree here and subtract, or to keep a
//! local list of "extensions we support". Either is a second source of truth for
//! a fact the extractor already owns, and the first time a grammar is added the
//! copy keeps confidently reporting the old languages — a stale claim rendered
//! as a fact, which is the failure mode this command exists to end.
//! [`Language::ALL`] travels on the report for the same reason: the client
//! prints the roster it is told, never one it remembers.
//!
//! # Repository scoping
//!
//! Every request names a filesystem path. [`CodeGraphOps::anchor`] resolves it
//! through the daemon's own [`crate::scan::discover_repository_root`] and hashes
//! the result with [`crate::scan::repository_id_for`] — the same derivation the
//! scan writes under, so a query can never land on an identity nothing was
//! stored beneath (2026-08-13 review, F5). Nothing on the wire lets a client
//! name a repository identity.
//!
//! That scope is applied where the **rows are fetched**, on every path. The
//! by-id read (`graph show --node <id>`) carries `AND repository = ?` in the
//! same statement as the lookup and answers
//! [`node_not_found`](codypendent_daemon::codegraph::node_not_found) for "not
//! yours" and "not there" alike. A gate enforced only where a list is built is
//! not a gate.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use codypendent_daemon::codegraph::{
    node_not_found, not_a_repository, BuildCodeGraphRequest, CodeGraphBuildFuture,
    CodeGraphGateway, CodeGraphReadFuture, CodeGraphReadRequest, CodeGraphStatusFuture,
    CodeGraphStatusRequest,
};
use codypendent_knowledge::{EvidenceRef, GitRevision, Language, ScanSummary};
use codypendent_protocol::{
    CodeGraphEdgeAssertion, CodeGraphEdgeView, CodeGraphGrammar, CodeGraphLanguageCount,
    CodeGraphNodeView, CodeGraphPage, CodeGraphQuery, CodeGraphScanReport,
    CodeGraphSkippedExtension, CodeGraphStatusView, CodeGraphTally, CodypendentError, RepositoryId,
};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};

#[path = "codegraph_locations.rs"]
pub(crate) mod locations;
use tracing::{info, warn};

use crate::scan;

/// The largest page `graph show` serves, and the page a request naming no limit
/// gets. Bounded for the reason [`MAX_SESSION_EVENTS_PAGE`] is: a repository
/// graph runs to hundreds of thousands of nodes, and the 16 MiB frame limit is a
/// wall, not a policy. A client narrows its filter or pages instead.
///
/// [`MAX_SESSION_EVENTS_PAGE`]: codypendent_daemon::server
const MAX_GRAPH_PAGE: u32 = 500;

/// The assembly's [`CodeGraphGateway`], over the daemon's pool.
///
/// It shares the executor's two registries by `Arc` rather than keeping its own:
/// an on-demand build must count as *the* fold for this revision (so the next
/// run does not redundantly re-scan) and must arm the live watcher exactly as a
/// session-opened fold does (so an edit after `graph build` still lands). A
/// private copy of either map would give this daemon two disagreeing opinions
/// about what has been folded and two watchers per checkout.
#[derive(Clone)]
pub struct CodeGraphOps {
    pool: SqlitePool,
    scanned: Arc<Mutex<HashMap<RepositoryId, GitRevision>>>,
    watchers: Arc<Mutex<HashMap<RepositoryId, scan::RepositoryWatcher>>>,
}

impl CodeGraphOps {
    #[must_use]
    pub fn new(
        pool: SqlitePool,
        scanned: Arc<Mutex<HashMap<RepositoryId, GitRevision>>>,
        watchers: Arc<Mutex<HashMap<RepositoryId, scan::RepositoryWatcher>>>,
    ) -> Self {
        Self {
            pool,
            scanned,
            watchers,
        }
    }

    /// Resolve a client-supplied directory to `(checkout root, repository id)`.
    ///
    /// The single derivation for all three commands. A path outside a checkout
    /// is refused rather than folded: recursively indexing a home or projects
    /// directory merges unrelated repositories into one graph, and answering
    /// "0 nodes" for it would be the same uninformative silence this module
    /// exists to remove.
    fn anchor(repository: &str) -> Result<(PathBuf, RepositoryId), CodypendentError> {
        let path = PathBuf::from(repository);
        let Some(root) = scan::discover_repository_root(&path) else {
            return Err(not_a_repository(repository));
        };
        let id = scan::repository_id_for(&root);
        Ok((root, id))
    }

    /// Arm the live watcher for a freshly folded checkout, once.
    ///
    /// Mirrors the executor's own `ensure_watching`, against the SAME registry:
    /// the registry entry is the "already watching" flag, so a second build
    /// re-uses the first one's watcher instead of leaking a notify thread per
    /// invocation. A watcher that cannot be armed is logged and skipped — the
    /// build still succeeded, and the graph then simply refreshes only when the
    /// revision moves.
    fn ensure_watching(&self, repository: RepositoryId, root: &Path) {
        let mut watchers = self.watchers.lock().expect("code-graph watcher registry");
        if watchers.contains_key(&repository) {
            return;
        }
        match scan::arm_watcher(self.pool.clone(), repository, root) {
            Ok(watcher) => {
                watchers.insert(repository, watcher);
            }
            Err(error) => warn!(
                %repository,
                %error,
                "could not arm the code-graph watcher after an on-demand build"
            ),
        }
    }
}

impl CodeGraphGateway for CodeGraphOps {
    fn build(&self, request: BuildCodeGraphRequest) -> CodeGraphBuildFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            let (root, repository) = Self::anchor(&request.repository)?;
            let started = Instant::now();

            // The graph's single writer gate — the same lock the startup scan,
            // the executor's warm-up, and the live watcher take. Without it an
            // on-demand build would race a session-opened one for the same
            // checkout: both clear, both rebuild, `database is locked`, and a
            // reader between one scanner's clear and its rebuild sees a torn
            // graph (2026-08-13 review, F6). Held across the whole scan.
            let guard = scan::lock_repository(repository).await;
            let scan_result = scan::scan_repository(&host.pool, repository, &root).await;
            drop(guard);
            let summary = scan_result.map_err(|error| {
                CodypendentError::new(
                    "graph.scan-failed",
                    format!("could not fold {}: {error}", root.display()),
                    true,
                )
            })?;

            // Record the fold under the SAME map the executor consults, and under
            // the SAME key it compares against (`scan::head_revision`), so the
            // next run for this checkout reuses this graph instead of rebuilding
            // it, and arm the watcher so edits made after the build still land.
            host.scanned
                .lock()
                .expect("scanned map lock")
                .insert(repository, scan::head_revision(&root));
            host.ensure_watching(repository, &root);

            // Per-language files/nodes/edges come from the graph tables rather
            // than the summary's file-only tally: they describe what the
            // repository now HOLDS, which is the question a build report is
            // read to answer, and they carry the edge counts the summary does
            // not track.
            let by_language = language_counts(&host.pool, repository).await?;
            let (nodes, edges) = totals(&host.pool, repository).await?;

            let report = CodeGraphScanReport {
                repository_root: root.display().to_string(),
                // The revision the rows actually CARRY, reported by the scan
                // rather than re-derived here: on a dirty checkout the scan folds
                // working-tree bytes and stamps `<HEAD>+workdir`, and a second
                // `git rev-parse` from this handler would print the bare commit
                // over rows that say otherwise.
                revision: summary.revision.clone(),
                files_walked: summary.files_seen as u64,
                files_supported: summary.files_supported as u64,
                files_folded: summary.files_folded as u64,
                files_unsupported: summary.files_skipped_unsupported as u64,
                files_ignored: summary.files_skipped_ignored as u64,
                nodes,
                edges,
                by_language,
                not_folded: rank_extensions(&summary),
                grammars: grammar_roster(),
                file_cap: summary.file_cap as u64,
                cap_hit: summary.truncated_by_cap,
                elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            };
            info!(
                %repository,
                revision = %report.revision,
                summary = %summary.headline(),
                "on-demand code-graph build complete"
            );
            Ok(report)
        })
    }

    fn status(&self, request: CodeGraphStatusRequest) -> CodeGraphStatusFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            let (root, repository) = Self::anchor(&request.repository)?;
            let (nodes, edges) = totals(&host.pool, repository).await?;
            let by_language = language_counts(&host.pool, repository).await?;
            let by_kind = tallies(
                &host.pool,
                repository,
                "SELECT kind AS label, COUNT(*) AS count FROM code_nodes \
                 WHERE repository = ? GROUP BY kind ORDER BY count DESC, label ASC",
            )
            .await?;
            let revisions = tallies(
                &host.pool,
                repository,
                "SELECT revision AS label, COUNT(*) AS count FROM code_nodes \
                 WHERE repository = ? GROUP BY revision ORDER BY count DESC, label ASC",
            )
            .await?;
            let files: i64 = sqlx::query_scalar(
                "SELECT COUNT(DISTINCT source_path) FROM code_nodes \
                 WHERE repository = ? AND source_path IS NOT NULL",
            )
            .bind(repository.to_string())
            .fetch_one(&host.pool)
            .await
            .map_err(store_error)?;

            let head = scan::head_revision(&root);
            let working_tree_dirty = scan::working_tree_dirty(&root);
            let (stale, stale_reason) = staleness(nodes, &revisions, &head.0, working_tree_dirty);

            Ok(CodeGraphStatusView {
                repository_root: root.display().to_string(),
                nodes,
                edges,
                files: files.max(0) as u64,
                by_language,
                by_kind,
                revisions,
                head_revision: head.0,
                working_tree_dirty,
                stale,
                stale_reason,
            })
        })
    }

    fn read(&self, request: CodeGraphReadRequest) -> CodeGraphReadFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            let (root, repository) = Self::anchor(&request.repository)?;
            let query = request.query;
            let limit = if query.limit == 0 {
                MAX_GRAPH_PAGE
            } else {
                query.limit.min(MAX_GRAPH_PAGE)
            };
            // Asking for neither is a client bug, not a legal "select nothing":
            // answer it as "everything" — BOTH halves, which is what
            // [`CodeGraphQuery::include_nodes`] documents. Both flags default to
            // `false`, so a protocol client sending the default `{}` lands here,
            // and answering it with nodes alone silently deleted every edge from
            // the reply of the most ordinary request there is.
            let (want_nodes, want_edges) = match (query.include_nodes, query.include_edges) {
                (false, false) => (true, true),
                pair => pair,
            };

            if query.connected_view
                && want_nodes
                && want_edges
                && query.node_id.is_none()
                && query.path.is_none()
                && query.language.is_none()
                && query.kind.is_none()
                && query.name.is_none()
            {
                let total_nodes = count_nodes(&host.pool, repository, &query).await?;
                let total_edges = count_edges(&host.pool, repository, &query).await?;
                let (nodes, edges) = if total_edges == 0 {
                    (
                        select_nodes(&host.pool, repository, &query, limit).await?,
                        Vec::new(),
                    )
                } else {
                    select_connected_page(&host.pool, repository, limit).await?
                };
                let mut page = CodeGraphPage {
                    nodes,
                    edges,
                    related_nodes: Vec::new(),
                    total_nodes,
                    total_edges,
                    limit,
                };
                locations::attach(&host.pool, repository, &root, &mut page).await?;
                return Ok(page);
            }

            // The node page is read even when only edges were asked for: this is
            // where the by-id gate lives, so `--node <id>` naming another
            // checkout is refused on that path too rather than quietly answered
            // with an empty edge list.
            let nodes = select_nodes(&host.pool, repository, &query, limit).await?;
            let total_nodes = count_nodes(&host.pool, repository, &query).await?;
            let (edges, total_edges) = if want_edges {
                select_edges(&host.pool, repository, &query, limit).await?
            } else {
                (Vec::new(), 0)
            };

            let mut page = CodeGraphPage {
                nodes: if want_nodes { nodes } else { Vec::new() },
                edges,
                related_nodes: Vec::new(),
                total_nodes,
                total_edges,
                limit,
            };
            if query.include_neighbors {
                page.related_nodes = select_related_nodes(&host.pool, repository, &page).await?;
            }
            locations::attach(&host.pool, repository, &root, &mut page).await?;
            Ok(page)
        })
    }
}

// ---------------------------------------------------------------------------
// Projecting the scan's own summary onto the wire
// ---------------------------------------------------------------------------

/// The scan's unsupported-extension histogram, ranked most-files-first.
///
/// The summary's map is already bounded by the extractor; this only imposes a
/// **total** order on it (files descending, then extension ascending) so two
/// builds over one tree render identically instead of reordering ties.
fn rank_extensions(summary: &ScanSummary) -> Vec<CodeGraphSkippedExtension> {
    let mut ranked: Vec<CodeGraphSkippedExtension> = summary
        .unsupported_by_extension
        .iter()
        .map(|(extension, files)| CodeGraphSkippedExtension {
            extension: extension.clone(),
            files: *files as u64,
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.files
            .cmp(&a.files)
            .then_with(|| a.extension.cmp(&b.extension))
    });
    ranked
}

/// The grammars this build carries, taken from the extractor's own roster.
///
/// Sent on every build report so the client can print "these extensions were
/// seen, these are the ones that would have worked" without keeping a copy of
/// the roster that goes stale the first time a grammar is added.
fn grammar_roster() -> Vec<CodeGraphGrammar> {
    Language::ALL
        .iter()
        .map(|language| CodeGraphGrammar {
            language: language.as_str().to_string(),
            extensions: language
                .extensions()
                .iter()
                .map(|extension| (*extension).to_string())
                .collect(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Reads over the stored graph. Every one is repository-scoped in SQL.
// ---------------------------------------------------------------------------

/// A store failure, as a wire error. Retryable: a locked database or a busy
/// scan is a "try again", not a permanent refusal.
fn store_error(error: sqlx::Error) -> CodypendentError {
    CodypendentError::new("graph.store-error", error.to_string(), true)
}

/// `(nodes, edges)` for one repository. Edges are scoped by joining `from_node`
/// back to the owning repository — an edge has no repository column of its own.
async fn totals(
    pool: &SqlitePool,
    repository: RepositoryId,
) -> Result<(u64, u64), CodypendentError> {
    let repo = repository.to_string();
    let nodes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM code_nodes WHERE repository = ?")
        .bind(&repo)
        .fetch_one(pool)
        .await
        .map_err(store_error)?;
    let edges: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM code_edges e JOIN code_nodes n ON e.from_node = n.id \
         WHERE n.repository = ?",
    )
    .bind(&repo)
    .fetch_one(pool)
    .await
    .map_err(store_error)?;
    Ok((nodes.max(0) as u64, edges.max(0) as u64))
}

/// Files/nodes/edges per stored language, most nodes first.
async fn language_counts(
    pool: &SqlitePool,
    repository: RepositoryId,
) -> Result<Vec<CodeGraphLanguageCount>, CodypendentError> {
    let repo = repository.to_string();
    let node_rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT language, COUNT(DISTINCT source_path), COUNT(*) FROM code_nodes \
         WHERE repository = ? GROUP BY language",
    )
    .bind(&repo)
    .fetch_all(pool)
    .await
    .map_err(store_error)?;
    let edge_rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT n.language, COUNT(*) FROM code_edges e JOIN code_nodes n ON e.from_node = n.id \
         WHERE n.repository = ? GROUP BY n.language",
    )
    .bind(&repo)
    .fetch_all(pool)
    .await
    .map_err(store_error)?;
    let edges_by_language: HashMap<String, i64> = edge_rows.into_iter().collect();

    let mut counts: Vec<CodeGraphLanguageCount> = node_rows
        .into_iter()
        .map(|(language, files, nodes)| CodeGraphLanguageCount {
            edges: edges_by_language
                .get(&language)
                .copied()
                .unwrap_or_default()
                .max(0) as u64,
            language,
            files: files.max(0) as u64,
            nodes: nodes.max(0) as u64,
        })
        .collect();
    counts.sort_by(|a, b| {
        b.nodes
            .cmp(&a.nodes)
            .then_with(|| a.language.cmp(&b.language))
    });
    Ok(counts)
}

/// Run a `SELECT <label>, <count>` grouped query bound to one repository.
async fn tallies(
    pool: &SqlitePool,
    repository: RepositoryId,
    sql: &str,
) -> Result<Vec<CodeGraphTally>, CodypendentError> {
    let rows: Vec<(String, i64)> = sqlx::query_as(sql)
        .bind(repository.to_string())
        .fetch_all(pool)
        .await
        .map_err(store_error)?;
    Ok(rows
        .into_iter()
        .map(|(label, count)| CodeGraphTally {
            label,
            count: count.max(0) as u64,
        })
        .collect())
}

/// Decide whether the stored graph still describes the working tree, and say why
/// in one sentence when it does not.
///
/// Four ways a graph goes stale, in the order a user cares about them: it is
/// empty; it was folded at a commit that is no longer `HEAD`; it holds rows
/// folded from uncommitted bytes (`<HEAD>+workdir`) that the tree no longer
/// has; or the tree has uncommitted edits and nothing in the graph carries the
/// `+workdir` stamp the live watcher writes for an uncommitted fold.
fn staleness(
    nodes: u64,
    revisions: &[CodeGraphTally],
    head: &str,
    working_tree_dirty: bool,
) -> (bool, Option<String>) {
    if nodes == 0 {
        return (
            true,
            Some("the graph is empty — nothing has been folded for this repository".to_string()),
        );
    }
    let workdir_stamp = format!("{head}+workdir");
    let mut foreign: Vec<&str> = revisions
        .iter()
        .map(|tally| tally.label.as_str())
        .filter(|revision| *revision != head && *revision != workdir_stamp)
        .collect();
    foreign.sort_unstable();
    if let Some(revision) = foreign.first() {
        return (
            true,
            Some(format!(
                "folded at {revision}, but HEAD is now {head} — run `codypendent graph build`"
            )),
        );
    }
    // The mirror image, and the half that was missing: a `+workdir` stamp says
    // "these rows describe bytes that were never committed". Revert those edits
    // and the bytes are gone, but `HEAD` never moved — so the filter above still
    // accepts the stamp and the dirty-tree check below is skipped, leaving
    // `graph status` reporting a graph of vanished source as `current`. Rows
    // refolded at a bare `HEAD` since then carry `HEAD`, not the stamp, so a
    // surviving `+workdir` tally is exactly the set that has not been refolded.
    //
    // Reachable in ordinary use, not only in tests: `build` treats a failure to
    // arm the live watcher as a successful build, so nothing is guaranteed to
    // refold the file when the edit is reverted.
    if !working_tree_dirty {
        if let Some(tally) = revisions
            .iter()
            .find(|tally| tally.label.ends_with("+workdir"))
        {
            return (
                true,
                Some(format!(
                    "{} symbol(s) were folded from uncommitted edits ({}) that the working tree \
                     no longer has — run `codypendent graph build`",
                    tally.count, tally.label
                )),
            );
        }
    }
    if working_tree_dirty
        && !revisions
            .iter()
            .any(|tally| tally.label.ends_with("+workdir"))
    {
        return (
            true,
            Some(
                "the working tree has uncommitted changes that no fold has covered yet".to_string(),
            ),
        );
    }
    (false, None)
}

/// Bind the repository scope and every narrowing filter onto a node query.
///
/// The repository predicate is pushed FIRST and unconditionally, on both the
/// list path and the by-id path, because it is the only one that is a
/// permission rather than a preference.
fn push_node_filters<'q>(
    builder: &mut QueryBuilder<'q, Sqlite>,
    repository: RepositoryId,
    query: &'q CodeGraphQuery,
) {
    builder
        .push(" WHERE repository = ")
        .push_bind(repository.to_string());
    if let Some(node_id) = &query.node_id {
        builder.push(" AND id = ").push_bind(node_id.as_str());
    }
    if let Some(path) = &query.path {
        builder
            .push(" AND source_path LIKE ")
            .push_bind(format!("{}%", escape_like(path)))
            .push(" ESCAPE '\\'");
    }
    if let Some(language) = &query.language {
        builder
            .push(" AND language = ")
            .push_bind(language.as_str());
    }
    if let Some(kind) = &query.kind {
        builder.push(" AND kind = ").push_bind(kind.as_str());
    }
    if let Some(name) = &query.name {
        builder
            .push(" AND qualified_name LIKE ")
            .push_bind(format!("%{}%", escape_like(name)))
            .push(" ESCAPE '\\'");
    }
}

/// Neutralize `LIKE`'s wildcards in user text, so a path or name containing `%`
/// narrows to itself instead of matching everything.
fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// One page of nodes matching `query`, repository-scoped.
///
/// A `node_id` that matches nothing **in this repository** returns the same
/// [`node_not_found`] as one that exists nowhere: the id is bound into the same
/// `WHERE repository = ? AND id = ?` as the scope, so the two cases are
/// literally the same empty result set and cannot be told apart downstream.
async fn select_nodes(
    pool: &SqlitePool,
    repository: RepositoryId,
    query: &CodeGraphQuery,
    limit: u32,
) -> Result<Vec<CodeGraphNodeView>, CodypendentError> {
    let mut builder = QueryBuilder::new(
        "SELECT id, language, package, source_path, qualified_name, kind, revision FROM code_nodes",
    );
    push_node_filters(&mut builder, repository, query);
    builder
        .push(" ORDER BY source_path ASC, qualified_name ASC, id ASC LIMIT ")
        .push_bind(i64::from(limit));
    let rows = builder.build().fetch_all(pool).await.map_err(store_error)?;
    let nodes: Vec<CodeGraphNodeView> = rows
        .into_iter()
        .map(|row| CodeGraphNodeView {
            id: row.get::<String, _>("id"),
            language: row.get::<String, _>("language"),
            package: row.get::<Option<String>, _>("package"),
            source_path: row.get::<Option<String>, _>("source_path"),
            qualified_name: row.get::<String, _>("qualified_name"),
            kind: row.get::<String, _>("kind"),
            revision: row.get::<String, _>("revision"),
            location: None,
        })
        .collect();
    if query.node_id.is_some() && nodes.is_empty() {
        return Err(node_not_found());
    }
    Ok(nodes)
}

async fn select_related_nodes(
    pool: &SqlitePool,
    repository: RepositoryId,
    page: &CodeGraphPage,
) -> Result<Vec<CodeGraphNodeView>, CodypendentError> {
    let known: HashSet<&str> = page.nodes.iter().map(|node| node.id.as_str()).collect();
    let mut ids: Vec<&str> = page
        .edges
        .iter()
        .flat_map(|edge| [edge.from_id.as_str(), edge.to_id.as_str()])
        .filter(|id| !known.contains(id))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut builder = QueryBuilder::new("SELECT id, language, package, source_path, qualified_name, kind, revision FROM code_nodes WHERE repository = ");
    builder
        .push_bind(repository.to_string())
        .push(" AND id IN (");
    let mut values = builder.separated(",");
    for id in ids {
        values.push_bind(id);
    }
    values.push_unseparated(") ORDER BY source_path, qualified_name, id");
    let rows = builder.build().fetch_all(pool).await.map_err(store_error)?;
    Ok(rows
        .into_iter()
        .map(|row| CodeGraphNodeView {
            id: row.get("id"),
            language: row.get("language"),
            package: row.get("package"),
            source_path: row.get("source_path"),
            qualified_name: row.get("qualified_name"),
            kind: row.get("kind"),
            revision: row.get("revision"),
            location: None,
        })
        .collect())
}

/// How many nodes match `query` before the limit, so a client can say "showing
/// 50 of 812". Shares [`push_node_filters`] with the page itself, so the count
/// and the page can never be scoped differently.
async fn count_nodes(
    pool: &SqlitePool,
    repository: RepositoryId,
    query: &CodeGraphQuery,
) -> Result<u64, CodypendentError> {
    let mut builder = QueryBuilder::new("SELECT COUNT(*) FROM code_nodes");
    push_node_filters(&mut builder, repository, query);
    let row = builder.build().fetch_one(pool).await.map_err(store_error)?;
    Ok(row.get::<i64, _>(0).max(0) as u64)
}

/// An opt-in overview is one real connected neighborhood, including endpoint metadata.
async fn select_connected_page(
    pool: &SqlitePool,
    repository: RepositoryId,
    limit: u32,
) -> Result<(Vec<CodeGraphNodeView>, Vec<CodeGraphEdgeView>), CodypendentError> {
    let repo = repository.to_string();
    let callable: Option<String> = sqlx::query_scalar(
        "SELECT n.id FROM code_nodes n WHERE n.repository = ? \
         AND n.kind IN ('function', 'method', 'test') AND n.source_path IS NOT NULL AND (\
           EXISTS (SELECT 1 FROM code_edges e JOIN code_nodes t ON t.id = e.to_node \
                   WHERE e.from_node = n.id AND t.repository = ? AND e.relation = 'calls') OR \
           EXISTS (SELECT 1 FROM code_edges e JOIN code_nodes f ON f.id = e.from_node \
                   WHERE e.to_node = n.id AND f.repository = ? AND e.relation = 'calls')) \
         ORDER BY CASE WHEN n.source_path LIKE 'src/%' THEN 0 ELSE 1 END, \
           ((SELECT COUNT(*) FROM code_edges e JOIN code_nodes f ON f.id = e.from_node \
             WHERE e.to_node = n.id AND f.repository = ? AND e.relation = 'calls') * 4 + \
            (SELECT COUNT(*) FROM code_edges e JOIN code_nodes t ON t.id = e.to_node \
             WHERE e.from_node = n.id AND t.repository = ? AND e.relation = 'calls')) DESC, \
           n.source_path, n.qualified_name, n.id LIMIT 1",
    )
    .bind(&repo)
    .bind(&repo)
    .bind(&repo)
    .bind(&repo)
    .bind(&repo)
    .fetch_optional(pool)
    .await
    .map_err(store_error)?;
    let seed = if callable.is_some() {
        callable
    } else {
        let seed: Option<String> = sqlx::query_scalar(
            "SELECT n.id FROM code_nodes n WHERE n.repository = ? AND (\
           EXISTS (SELECT 1 FROM code_edges e JOIN code_nodes t ON t.id = e.to_node \
                   WHERE e.from_node = n.id AND t.repository = ?) OR \
           EXISTS (SELECT 1 FROM code_edges e JOIN code_nodes f ON f.id = e.from_node \
                   WHERE e.to_node = n.id AND f.repository = ?)) \
         ORDER BY CASE \
           WHEN n.kind IN ('file', 'module') AND n.source_path LIKE 'src/%' THEN 0 \
           WHEN n.kind IN ('file', 'module') AND n.source_path IS NOT NULL \
             AND n.source_path NOT LIKE '.%' AND n.source_path NOT LIKE 'vendor/%' \
             AND n.source_path NOT LIKE 'node_modules/%' AND n.source_path NOT LIKE 'target/%' \
             AND n.source_path NOT LIKE 'tests/%' THEN 1 ELSE 2 END, \
           ((SELECT COUNT(*) FROM code_edges e JOIN code_nodes t ON t.id = e.to_node \
             WHERE e.from_node = n.id AND t.repository = ?) + \
            (SELECT COUNT(*) FROM code_edges e JOIN code_nodes f ON f.id = e.from_node \
             WHERE e.to_node = n.id AND f.repository = ?)) DESC, \
           n.source_path ASC, n.qualified_name ASC, n.id ASC LIMIT 1",
        )
        .bind(&repo)
        .bind(&repo)
        .bind(&repo)
        .bind(&repo)
        .bind(&repo)
        .fetch_optional(pool)
        .await
        .map_err(store_error)?;
        seed
    };
    let Some(seed) = seed else {
        return Ok((
            select_nodes(pool, repository, &CodeGraphQuery::default(), limit).await?,
            Vec::new(),
        ));
    };
    let mut nodes = select_nodes(
        pool,
        repository,
        &CodeGraphQuery {
            node_id: Some(seed.clone()),
            ..Default::default()
        },
        1,
    )
    .await?;
    let mut known = HashSet::from([seed.clone()]);
    let mut frontier = VecDeque::from([seed]);
    let mut seen_edges = HashSet::new();
    let mut edges = Vec::new();
    while let Some(id) = frontier.pop_front() {
        if edges.len() >= limit as usize {
            break;
        }
        let rows = sqlx::query(
            "SELECT e.id AS edge_id, e.from_node, f.qualified_name AS from_name, \
                    e.to_node, t.qualified_name AS to_name, e.relation, e.confidence, \
                    e.evidence_kind, e.evidence_artifact, e.revision, \
                    f.language AS from_language, f.package AS from_package, \
                    f.source_path AS from_path, f.kind AS from_kind, f.revision AS from_revision, \
                    t.language AS to_language, t.package AS to_package, \
                    t.source_path AS to_path, t.kind AS to_kind, t.revision AS to_revision \
             FROM code_edges e JOIN code_nodes f ON f.id = e.from_node \
             JOIN code_nodes t ON t.id = e.to_node \
             WHERE f.repository = ? AND t.repository = ? AND (e.from_node = ? OR e.to_node = ?) \
             ORDER BY CASE e.relation WHEN 'calls' THEN 0 WHEN 'contains' THEN 1 WHEN 'defines' THEN 2 ELSE 3 END, \
               CASE WHEN e.from_node = ? THEN 0 ELSE 1 END, \
               f.source_path, t.source_path, f.qualified_name, e.relation, t.qualified_name, e.id \
             LIMIT ?",
        )
        .bind(&repo)
        .bind(&repo)
        .bind(&id)
        .bind(&id)
        .bind(&id)
        .bind(i64::from(limit))
        .fetch_all(pool)
        .await
        .map_err(store_error)?;
        for row in rows {
            let edge_id: String = row.get("edge_id");
            if seen_edges.contains(&edge_id) {
                continue;
            }
            let from = incident_node(&row, "from");
            let to = incident_node(&row, "to");
            let missing = usize::from(!known.contains(&from.id))
                + usize::from(from.id != to.id && !known.contains(&to.id));
            if nodes.len() + missing > limit as usize {
                continue;
            }
            for node in [from, to] {
                if known.insert(node.id.clone()) {
                    frontier.push_back(node.id.clone());
                    nodes.push(node);
                }
            }
            seen_edges.insert(edge_id);
            edges.push(edge_view(&row));
            if edges.len() >= limit as usize {
                break;
            }
        }
    }
    Ok((nodes, edges))
}

fn incident_node(row: &sqlx::sqlite::SqliteRow, prefix: &str) -> CodeGraphNodeView {
    CodeGraphNodeView {
        id: row.get(format!("{prefix}_node").as_str()),
        qualified_name: row.get(format!("{prefix}_name").as_str()),
        language: row.get(format!("{prefix}_language").as_str()),
        package: row.get(format!("{prefix}_package").as_str()),
        source_path: row.get(format!("{prefix}_path").as_str()),
        kind: row.get(format!("{prefix}_kind").as_str()),
        revision: row.get(format!("{prefix}_revision").as_str()),
        location: None,
    }
}

/// The edges incident to the nodes matching `query`, with both endpoints named,
/// and its own page of at most `limit` rows.
///
/// The incident set is the **complete** filtered node set, pushed down as a
/// subquery over the same [`push_node_filters`] the node page uses — never the
/// node page itself. Taking the ids from the already-limited page dropped every
/// edge whose two endpoints both fell past the page boundary, and undercounted
/// `total_edges` while the client renders that number as a pre-limit total
/// ("showing N of M"). `limit` is documented as a maximum per row *kind*, so
/// nodes and edges are paged independently rather than one riding on the other.
///
/// **Both** endpoints are joined back to `code_nodes` and both are constrained
/// to the repository. Scoping only `from_node` — which is what
/// [`codypendent_knowledge::codegraph::edges`] does for its own whole-repository
/// read, correctly, since there the two cannot differ — would here let an edge
/// name a node outside the caller's repository in `to_name`, which is precisely
/// the leak the by-id gate exists to prevent, arriving by a different door.
async fn select_edges(
    pool: &SqlitePool,
    repository: RepositoryId,
    query: &CodeGraphQuery,
    limit: u32,
) -> Result<(Vec<CodeGraphEdgeView>, u64), CodypendentError> {
    let total = count_edges(pool, repository, query).await?;

    // `evidence_artifact` joins the page because `evidence_kind` alone says only
    // THAT an edge was asserted by an agent, never by whom or why — and
    // `graph.assert_edge` records exactly that, run and required rationale, into
    // this column on every write. Selecting the column is what turns the
    // agent-write audit trail from stored-but-unreadable into something a client
    // can render (see `agent_assertion`).
    let mut builder = QueryBuilder::new(
        "SELECT e.from_node, f.qualified_name AS from_name, e.to_node, t.qualified_name AS to_name, \
                e.relation, e.confidence, e.evidence_kind, e.evidence_artifact, e.revision \
         FROM code_edges e \
         JOIN code_nodes f ON e.from_node = f.id \
         JOIN code_nodes t ON e.to_node = t.id",
    );
    push_edge_filters(&mut builder, repository, query);
    builder
        .push(" ORDER BY f.qualified_name ASC, e.relation ASC, t.qualified_name ASC LIMIT ")
        .push_bind(i64::from(limit));

    let rows = builder.build().fetch_all(pool).await.map_err(store_error)?;
    Ok((rows.iter().map(edge_view).collect(), total))
}

async fn count_edges(
    pool: &SqlitePool,
    repository: RepositoryId,
    query: &CodeGraphQuery,
) -> Result<u64, CodypendentError> {
    // The count runs first, over the SAME predicate builder as the page and
    // without its `LIMIT`, so `total_edges` is the real total rather than
    // "however many the limit let through". Reporting a clamped page size as the
    // total is the same class of confidently-wrong number as the bare zero this
    // whole command family exists to replace.
    let mut counter = QueryBuilder::new(
        "SELECT COUNT(*) FROM code_edges e \
         JOIN code_nodes f ON e.from_node = f.id \
         JOIN code_nodes t ON e.to_node = t.id",
    );
    push_edge_filters(&mut counter, repository, query);
    let total = counter
        .build()
        .fetch_one(pool)
        .await
        .map_err(store_error)?
        .get::<i64, _>(0)
        .max(0) as u64;

    Ok(total)
}

fn edge_view(row: &sqlx::sqlite::SqliteRow) -> CodeGraphEdgeView {
    CodeGraphEdgeView {
        from_id: row.get::<String, _>("from_node"),
        from_name: row.get::<String, _>("from_name"),
        to_id: row.get::<String, _>("to_node"),
        to_name: row.get::<String, _>("to_name"),
        relation: row.get::<String, _>("relation"),
        confidence: row.get::<f64, _>("confidence") as f32,
        evidence_kind: row.get::<String, _>("evidence_kind"),
        revision: row.get::<String, _>("revision"),
        call_site: None,
        asserted_by: agent_assertion(row.get::<Option<String>, _>("evidence_artifact").as_deref()),
    }
}

/// The agent-assertion provenance stored in one edge's `evidence_artifact`, if
/// that is what it holds.
///
/// `EvidenceRef` has three variants and only [`EvidenceRef::AgentAssertion`]
/// points at a judgement rather than at bytes; the other two describe a
/// machine-derived edge and have nothing a reviewer needs to read, so they
/// project to `None` rather than to an empty assertion. Unparseable or absent
/// evidence is likewise `None` — a stored row this daemon cannot interpret must
/// not stop the page being served, and an edge with no readable provenance is
/// honestly reported as having none.
fn agent_assertion(evidence_json: Option<&str>) -> Option<CodeGraphEdgeAssertion> {
    let evidence: EvidenceRef = serde_json::from_str(evidence_json?).ok()?;
    match evidence {
        EvidenceRef::AgentAssertion {
            session_id,
            run_id,
            rationale,
        } => Some(CodeGraphEdgeAssertion {
            session_id,
            run_id,
            rationale,
        }),
        EvidenceRef::EventRange { .. } | EvidenceRef::Artifact { .. } => None,
    }
}

/// Bind the repository scope and the incident-node predicate onto an edge query.
///
/// Shared by the page and its count so the two can never be scoped differently —
/// the same reason [`push_node_filters`] is shared. The incident set is a
/// subquery over `code_nodes` carrying the caller's filters, so it is the set the
/// filter selects rather than the slice of it one page happened to show.
///
/// **Both** endpoints are constrained to the repository. Scoping only
/// `from_node` — which is what [`codypendent_knowledge::codegraph::edges`] does
/// for its own whole-repository read, correctly, since there the two cannot
/// differ — would here let an edge name a node outside the caller's repository
/// in `to_name`, which is precisely the leak the by-id gate exists to prevent,
/// arriving through a different door.
fn push_edge_filters<'q>(
    builder: &mut QueryBuilder<'q, Sqlite>,
    repository: RepositoryId,
    query: &'q CodeGraphQuery,
) {
    let repo = repository.to_string();
    builder
        .push(" WHERE f.repository = ")
        .push_bind(repo.clone());
    builder.push(" AND t.repository = ").push_bind(repo);
    builder.push(" AND (e.from_node IN (SELECT id FROM code_nodes");
    push_node_filters(builder, repository, query);
    builder.push(") OR e.to_node IN (SELECT id FROM code_nodes");
    push_node_filters(builder, repository, query);
    builder.push("))");
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::process::Command;

    use codypendent_knowledge::codegraph;

    fn init_repo(path: &Path) {
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
        ] {
            let status = Command::new("git")
                .current_dir(path)
                .args(&args)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        }
    }

    /// A migrated pool on disk. Not `:memory:` — `open_database` treats its
    /// argument as a path, so an in-memory URL would give each connection in the
    /// pool its own empty database and no migrated tables at all.
    async fn pool(dir: &tempfile::TempDir) -> SqlitePool {
        codypendent_daemon::db::open_database(&dir.path().join("codypendent.db"))
            .await
            .expect("open db")
    }

    fn ops(pool: SqlitePool) -> CodeGraphOps {
        CodeGraphOps::new(
            pool,
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
        )
    }

    #[tokio::test]
    async fn connected_view_returns_one_real_application_neighborhood_with_full_metadata() {
        let data = tempfile::tempdir().unwrap();
        let pool = pool(&data).await;
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let repository = scan::repository_id_for(repo.path());
        let revision = GitRevision("r1".into());
        for index in 0..20 {
            codegraph::upsert_file_graph(
                &pool,
                repository,
                &revision,
                &format!("aaa/pad{index}.rs"),
                "// only a file node\n",
            )
            .await
            .unwrap();
        }
        for (path, source) in [
            (
                "src/main.rs",
                "pub fn entry() { work(); }\npub fn work() {}\n",
            ),
            (
                "vendor/large.rs",
                "pub fn a() {}\npub fn b() {}\npub fn c() {}\npub fn d() {}\n",
            ),
            (
                "tools/other.rs",
                "pub fn a() {}\npub fn b() {}\npub fn c() {}\n",
            ),
        ] {
            codegraph::upsert_file_graph(&pool, repository, &revision, path, source)
                .await
                .unwrap();
        }
        let ops = ops(pool.clone());
        let request = || CodeGraphReadRequest {
            repository: repo.path().display().to_string(),
            query: CodeGraphQuery {
                connected_view: true,
                include_nodes: true,
                include_edges: true,
                limit: 100,
                ..Default::default()
            },
        };
        let page = ops.read(request()).await.unwrap();
        assert_eq!(
            page.nodes[0].qualified_name, "work",
            "The called source function should anchor the overview"
        );
        assert_eq!(page.nodes.len(), 3, "{page:?}");
        assert_eq!(page.edges.len(), 5, "{page:?}");
        assert!(page
            .nodes
            .iter()
            .all(|node| node.source_path.as_deref() == Some("src/main.rs")
                && node.language == "rust"
                && !node.kind.is_empty()));
        let ids: HashSet<_> = page.nodes.iter().map(|node| node.id.as_str()).collect();
        assert!(page
            .edges
            .iter()
            .all(|edge| ids.contains(edge.from_id.as_str()) && ids.contains(edge.to_id.as_str())));
        let totals = totals(&pool, repository).await.unwrap();
        assert_eq!((page.total_nodes, page.total_edges), totals);
        assert!(page.total_nodes > page.nodes.len() as u64);
        assert_eq!(ops.read(request()).await.unwrap(), page);
    }

    #[tokio::test]
    async fn connected_view_respects_both_budgets_and_keeps_explicit_filters_independent() {
        let data = tempfile::tempdir().unwrap();
        let pool = pool(&data).await;
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let repository = scan::repository_id_for(repo.path());
        let revision = GitRevision("r1".into());
        let source: String = (0..60)
            .map(|index| format!("pub fn f{index:02}() {{}}\n"))
            .collect();
        codegraph::upsert_file_graph(&pool, repository, &revision, "src/main.rs", &source)
            .await
            .unwrap();
        let ops = ops(pool.clone());
        for limit in [1, 7, 100] {
            let page = ops
                .read(CodeGraphReadRequest {
                    repository: repo.path().display().to_string(),
                    query: CodeGraphQuery {
                        connected_view: true,
                        include_nodes: true,
                        include_edges: true,
                        limit,
                        ..Default::default()
                    },
                })
                .await
                .unwrap();
            assert!(
                page.nodes.len() <= limit as usize && page.edges.len() <= limit as usize,
                "{page:?}"
            );
            let ids: HashSet<_> = page.nodes.iter().map(|node| node.id.as_str()).collect();
            assert!(page.edges.iter().all(
                |edge| ids.contains(edge.from_id.as_str()) && ids.contains(edge.to_id.as_str())
            ));
            assert_eq!(page.total_nodes, 61);
            assert_eq!(page.total_edges, 120);
        }
        let filtered = |connected_view| CodeGraphReadRequest {
            repository: repo.path().display().to_string(),
            query: CodeGraphQuery {
                connected_view,
                name: Some("f01".into()),
                include_nodes: true,
                include_edges: true,
                limit: 7,
                ..Default::default()
            },
        };
        let legacy = ops.read(filtered(false)).await.unwrap();
        assert_eq!(ops.read(filtered(true)).await.unwrap(), legacy);
        assert_eq!(legacy.nodes.len(), 1);
        assert!(!legacy.edges.is_empty());
    }

    #[tokio::test]
    async fn connected_view_falls_back_to_real_nodes_when_no_relationships_exist() {
        let data = tempfile::tempdir().unwrap();
        let pool = pool(&data).await;
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let repository = scan::repository_id_for(repo.path());
        codegraph::upsert_file_graph(
            &pool,
            repository,
            &GitRevision("r1".into()),
            "src/empty.rs",
            "// no relationships\n",
        )
        .await
        .unwrap();
        let page = ops(pool)
            .read(CodeGraphReadRequest {
                repository: repo.path().display().to_string(),
                query: CodeGraphQuery {
                    connected_view: true,
                    include_nodes: true,
                    include_edges: true,
                    limit: 100,
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        assert_eq!(page.nodes.len(), 1);
        assert_eq!(page.total_nodes, 1);
        assert!(page.edges.is_empty());
        assert_eq!(page.total_edges, 0);
    }

    #[tokio::test]
    async fn graph_locations_include_verified_definitions_call_sites_and_related_metadata() {
        let data = tempfile::tempdir().unwrap();
        let pool = pool(&data).await;
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        let source = "// π heading\npub fn target() {}\n\npub fn caller() {\n    let _ = \"é\"; target();\n    target();\n}\n";
        std::fs::write(repo.path().join("src/main.rs"), source).unwrap();
        let repository = scan::repository_id_for(repo.path());
        codegraph::upsert_file_graph(
            &pool,
            repository,
            &GitRevision("r1".into()),
            "src/main.rs",
            source,
        )
        .await
        .unwrap();
        let target: String = sqlx::query_scalar(
            "SELECT id FROM code_nodes WHERE repository = ? AND qualified_name = 'target'",
        )
        .bind(repository.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        let ops = ops(pool);
        let request = || CodeGraphReadRequest {
            repository: repo.path().display().to_string(),
            query: CodeGraphQuery {
                node_id: Some(target.clone()),
                include_nodes: true,
                include_edges: true,
                include_neighbors: true,
                limit: 100,
                ..Default::default()
            },
        };
        let page = ops.read(request()).await.unwrap();
        assert_eq!(page.nodes.len(), 1);
        assert_eq!(
            page.total_nodes, 1,
            "Endpoint metadata must not inflate filter matches"
        );
        assert_eq!(
            page.nodes[0]
                .location
                .as_ref()
                .map(|site| (site.line, site.column)),
            Some((2, 1))
        );
        let caller = page
            .related_nodes
            .iter()
            .find(|node| node.qualified_name == "caller")
            .unwrap();
        assert_eq!(
            caller
                .location
                .as_ref()
                .map(|site| (site.line, site.column)),
            Some((4, 1))
        );
        let calls: Vec<_> = page
            .edges
            .iter()
            .filter(|edge| edge.relation == "calls")
            .collect();
        assert_eq!(
            calls.len(),
            1,
            "The durable triple retains one recorded call occurrence"
        );
        let site = calls[0].call_site.as_ref().unwrap();
        assert_eq!(
            (site.source_path.as_str(), site.line, site.column),
            ("src/main.rs", 5, 18)
        );
        let ids: HashSet<_> = page
            .nodes
            .iter()
            .chain(&page.related_nodes)
            .map(|node| node.id.as_str())
            .collect();
        assert!(page
            .edges
            .iter()
            .all(|edge| ids.contains(edge.from_id.as_str()) && ids.contains(edge.to_id.as_str())));
        std::fs::write(
            repo.path().join("src/main.rs"),
            format!("// changed\n{source}"),
        )
        .unwrap();
        let stale = ops.read(request()).await.unwrap();
        assert!(stale
            .nodes
            .iter()
            .chain(&stale.related_nodes)
            .all(|node| node.location.is_none()));
        assert!(stale.edges.iter().all(|edge| edge.call_site.is_none()));
    }

    /// An extension histogram is ranked most-files-first with a total order, so
    /// two builds over one tree render identically instead of reordering ties.
    #[test]
    fn the_extension_histogram_ranks_by_files_then_name() {
        let mut summary = ScanSummary::default();
        for (extension, count) in [("go", 3), ("md", 3), ("toml", 9)] {
            for _ in 0..count {
                summary.record_unsupported(Path::new(&format!("a.{extension}")));
            }
        }
        let ranked = rank_extensions(&summary);
        let order: Vec<&str> = ranked.iter().map(|row| row.extension.as_str()).collect();
        assert_eq!(order, vec!["toml", "go", "md"]);
    }

    /// The roster travels on the report, straight from the extractor. A client
    /// that printed its own list would keep naming the old languages the first
    /// time a grammar is added — the stale-claim-as-fact failure this whole
    /// command family exists to end.
    #[test]
    fn the_grammar_roster_comes_from_the_extractor() {
        let roster = grammar_roster();
        assert_eq!(roster.len(), Language::ALL.len());
        let rust = roster
            .iter()
            .find(|grammar| grammar.language == "rust")
            .expect("rust is in the roster");
        assert_eq!(rust.extensions, vec!["rs".to_string()]);
    }

    /// A path or name carrying `LIKE`'s wildcards must narrow to itself. Without
    /// the escape, `graph show --path %` matched every file in the repository
    /// while claiming to be filtered.
    #[test]
    fn like_wildcards_in_user_text_are_neutralized() {
        assert_eq!(escape_like("a%b_c"), "a\\%b\\_c");
        assert_eq!(escape_like("100\\%"), "100\\\\\\%");
    }

    /// Staleness has to name the reason, because "stale: true" is the same
    /// unhelpful silence as "0 nodes".
    #[test]
    fn staleness_explains_each_of_its_four_causes() {
        let empty = staleness(0, &[], "abc", false);
        assert!(empty.0 && empty.1.expect("reason").contains("empty"));

        let old = staleness(
            5,
            &[CodeGraphTally {
                label: "old".to_string(),
                count: 5,
            }],
            "abc",
            false,
        );
        assert!(old.0);
        assert!(old.1.expect("reason").contains("HEAD is now abc"));

        let dirty = staleness(
            5,
            &[CodeGraphTally {
                label: "abc".to_string(),
                count: 5,
            }],
            "abc",
            true,
        );
        assert!(dirty.0);
        assert!(dirty.1.expect("reason").contains("uncommitted"));

        // A graph carrying the watcher's `+workdir` stamp already describes the
        // uncommitted tree, so a dirty tree alone must not report it stale.
        let folded_dirty = staleness(
            5,
            &[CodeGraphTally {
                label: "abc+workdir".to_string(),
                count: 5,
            }],
            "abc",
            true,
        );
        assert!(!folded_dirty.0, "{folded_dirty:?}");
        assert!(folded_dirty.1.is_none());

        // And the fourth: the SAME stamp against a tree that is now clean. The
        // rows describe uncommitted bytes; the tree no longer has them. `HEAD`
        // never moved, so nothing else in this function notices.
        let reverted = staleness(
            5,
            &[CodeGraphTally {
                label: "abc+workdir".to_string(),
                count: 5,
            }],
            "abc",
            false,
        );
        assert!(reverted.0, "{reverted:?}");
        let reason = reverted.1.expect("reason");
        assert!(reason.contains("uncommitted"), "{reason}");
        assert!(reason.contains("no longer has"), "{reason}");

        // Refolding at the bare commit clears it: the rows now carry `HEAD`, so
        // there is no `+workdir` tally left to be stale.
        let refolded = staleness(
            5,
            &[CodeGraphTally {
                label: "abc".to_string(),
                count: 5,
            }],
            "abc",
            false,
        );
        assert!(!refolded.0, "{refolded:?}");
    }

    /// A directory outside a checkout is refused rather than answered "empty".
    /// Answering "0 nodes" for a home directory is the same misleading silence
    /// this command family exists to remove.
    #[tokio::test]
    async fn a_directory_outside_a_checkout_is_refused() {
        let plain = tempfile::tempdir().expect("tempdir");
        let data = tempfile::tempdir().expect("tempdir");
        let error = ops(pool(&data).await)
            .status(CodeGraphStatusRequest {
                repository: plain.path().display().to_string(),
            })
            .await
            .expect_err("a non-checkout has no graph");
        assert_eq!(error.code, "graph.not-a-repository");
    }

    /// **The gate.** `graph show --node <id>` must not become a way to read
    /// another repository's graph. A node id that exists — in a different
    /// checkout — is refused with byte-identical bytes to an id that exists
    /// nowhere, so naming an id can never confirm it.
    #[tokio::test]
    async fn a_node_id_from_another_repository_is_refused_identically_to_a_missing_one() {
        let data = tempfile::tempdir().expect("tempdir");
        let pool = pool(&data).await;
        let mine = tempfile::tempdir().expect("tempdir");
        let theirs = tempfile::tempdir().expect("tempdir");
        init_repo(mine.path());
        init_repo(theirs.path());
        let my_id = scan::repository_id_for(mine.path());
        let their_id = scan::repository_id_for(theirs.path());
        assert_ne!(my_id, their_id);

        let revision = GitRevision("r1".to_string());
        codegraph::upsert_file_graph(
            &pool,
            their_id,
            &revision,
            "src/lib.rs",
            "pub fn hidden() {}",
        )
        .await
        .expect("fold the other repository");
        let secret = select_nodes(&pool, their_id, &CodeGraphQuery::default(), 50)
            .await
            .expect("read the other repository directly")
            .into_iter()
            .find(|node| node.qualified_name.contains("hidden"))
            .expect("the other repository really does hold this node");

        let ops = ops(pool);
        let by_real_id = ops
            .read(CodeGraphReadRequest {
                repository: mine.path().display().to_string(),
                query: CodeGraphQuery {
                    node_id: Some(secret.id.clone()),
                    ..CodeGraphQuery::default()
                },
            })
            .await
            .expect_err("another repository's node must not be readable");
        let by_absent_id = ops
            .read(CodeGraphReadRequest {
                repository: mine.path().display().to_string(),
                query: CodeGraphQuery {
                    node_id: Some("00000000-0000-0000-0000-000000000000".to_string()),
                    ..CodeGraphQuery::default()
                },
            })
            .await
            .expect_err("an absent node must not be readable either");

        assert_eq!(by_real_id.code, by_absent_id.code);
        assert_eq!(by_real_id.message, by_absent_id.message);
        assert_eq!(by_real_id.code, "graph.node-not-found");
    }

    /// The same gate on the LIST path: a query with no filters at all sees only
    /// its own repository's rows.
    #[tokio::test]
    async fn an_unfiltered_list_is_still_repository_scoped() {
        let data = tempfile::tempdir().expect("tempdir");
        let pool = pool(&data).await;
        let mine = tempfile::tempdir().expect("tempdir");
        let theirs = tempfile::tempdir().expect("tempdir");
        init_repo(mine.path());
        init_repo(theirs.path());
        let revision = GitRevision("r1".to_string());
        codegraph::upsert_file_graph(
            &pool,
            scan::repository_id_for(theirs.path()),
            &revision,
            "src/lib.rs",
            "pub fn hidden() {}",
        )
        .await
        .expect("fold the other repository");

        let page = ops(pool)
            .read(CodeGraphReadRequest {
                repository: mine.path().display().to_string(),
                query: CodeGraphQuery::default(),
            })
            .await
            .expect("an empty graph is a legal answer");
        assert_eq!(page.total_nodes, 0, "{page:?}");
        assert!(page.nodes.is_empty(), "{page:?}");
    }

    /// The total a page reports must be the real total, not the clamped page
    /// size. "showing 1 of 1" for a graph holding twelve edges is the same
    /// species of confidently-wrong number as the bare zero.
    #[tokio::test]
    async fn an_edge_page_reports_the_true_total_not_the_page_size() {
        let data = tempfile::tempdir().expect("tempdir");
        let pool = pool(&data).await;
        let repo = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path());
        codegraph::upsert_file_graph(
            &pool,
            scan::repository_id_for(repo.path()),
            &GitRevision("r1".to_string()),
            "src/lib.rs",
            "pub fn a() { b(); }\npub fn b() {}\n",
        )
        .await
        .expect("fold");

        let page = ops(pool)
            .read(CodeGraphReadRequest {
                repository: repo.path().display().to_string(),
                query: CodeGraphQuery {
                    include_edges: true,
                    include_nodes: true,
                    limit: 1,
                    ..CodeGraphQuery::default()
                },
            })
            .await
            .expect("read");
        assert_eq!(page.edges.len(), 1, "the page is clamped: {page:?}");
        assert!(
            page.total_edges > 1,
            "the total must be the real total, not the page size: {page:?}"
        );
    }

    /// **The default request.** Both flags are `#[serde(default)]` bools, so a
    /// protocol client sending `{}` arrives with `(false, false)` — the "asking
    /// for neither" case. The contract calls that everything; the handler used
    /// to answer it with nodes and no edges, so the most ordinary request there
    /// is silently rendered an edgeless graph.
    #[tokio::test]
    async fn a_query_asking_for_neither_half_is_answered_with_both() {
        let data = tempfile::tempdir().expect("tempdir");
        let pool = pool(&data).await;
        let repo = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path());
        codegraph::upsert_file_graph(
            &pool,
            scan::repository_id_for(repo.path()),
            &GitRevision("r1".to_string()),
            "src/lib.rs",
            "pub fn a() { b(); }\npub fn b() {}\n",
        )
        .await
        .expect("fold");

        let query: CodeGraphQuery = serde_json::from_str("{}").expect("the wire default");
        assert!(!query.include_nodes && !query.include_edges);
        let ops = ops(pool);
        let page = ops
            .read(CodeGraphReadRequest {
                repository: repo.path().display().to_string(),
                query,
            })
            .await
            .expect("read");
        assert!(!page.nodes.is_empty(), "{page:?}");
        assert!(
            !page.edges.is_empty(),
            "an omitted flag must not delete the edges: {page:?}"
        );
        assert!(page.total_edges > 0, "{page:?}");

        // An explicit half is still honoured — "neither" is the only case the
        // handler reinterprets, so asking for edges alone must not smuggle the
        // node page back in.
        let edges_only = ops
            .read(CodeGraphReadRequest {
                repository: repo.path().display().to_string(),
                query: CodeGraphQuery {
                    include_edges: true,
                    ..CodeGraphQuery::default()
                },
            })
            .await
            .expect("read");
        assert!(edges_only.nodes.is_empty(), "{edges_only:?}");
        assert_eq!(edges_only.edges.len(), page.edges.len(), "{edges_only:?}");
        assert_eq!(edges_only.total_nodes, page.total_nodes);
    }

    /// **Edges are not a rider on the node page.** `limit` is a maximum per row
    /// KIND, so an edge whose two endpoints both fall past the node page must
    /// still be returned and still be counted — the CLI renders `total_edges` as
    /// the `M` in "showing N of M", so a total narrowed to one page's nodes is a
    /// wrong number printed as a fact.
    #[tokio::test]
    async fn edges_span_the_whole_filtered_node_set_not_the_node_page() {
        let data = tempfile::tempdir().expect("tempdir");
        let pool = pool(&data).await;
        let repo = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path());
        let repository = scan::repository_id_for(repo.path());
        let revision = GitRevision("r1".to_string());
        // Padding that contributes nodes and no edges, sorting before the one
        // file that holds a call — so with a small limit the calling pair is
        // entirely past the node page, and every edge in the repository belongs
        // to nodes the page never shows.
        for index in 1..=6 {
            codegraph::upsert_file_graph(
                &pool,
                repository,
                &revision,
                &format!("src/pad{index:02}.rs"),
                "// nothing to define\n",
            )
            .await
            .expect("fold the padding");
        }
        codegraph::upsert_file_graph(
            &pool,
            repository,
            &revision,
            "src/zzz.rs",
            "pub fn zzz_caller() { zzz_callee(); }\npub fn zzz_callee() {}\n",
        )
        .await
        .expect("fold the pair");

        let page = ops(pool.clone())
            .read(CodeGraphReadRequest {
                repository: repo.path().display().to_string(),
                query: CodeGraphQuery {
                    include_edges: true,
                    include_nodes: true,
                    // Five of the six padding nodes, so the calling pair sits
                    // past the node page — and five edge rows, which is the
                    // whole repository's edge set.
                    limit: 5,
                    ..CodeGraphQuery::default()
                },
            })
            .await
            .expect("read");
        assert_eq!(page.nodes.len(), 5, "the node page is clamped: {page:?}");
        assert!(
            page.nodes
                .iter()
                .all(|node| node.source_path.as_deref() != Some("src/zzz.rs")),
            "the pair must be past the node page for this to prove anything: {page:?}"
        );

        // The edges of the whole filtered node set — not of the three nodes that
        // fitted on the page, which are incident to nothing.
        let (_, true_edges) = totals(&pool, repository).await.expect("totals");
        assert_eq!(
            page.total_edges, true_edges,
            "total_edges must be the filter's true total: {page:?}"
        );
        assert!(page.total_edges > 0, "{page:?}");
        assert!(
            page.edges
                .iter()
                .any(|edge| edge.from_name.contains("zzz_caller")
                    && edge.to_name.contains("zzz_callee")),
            "the call between two off-page nodes must still be returned: {page:?}"
        );
    }

    /// The edge read is the other door into another repository's rows: an edge
    /// names TWO nodes and both names are rendered verbatim, so the scope has to
    /// hold on `to_node` as well as `from_node`.
    ///
    /// The cross-repository edge is inserted directly, because `upsert_file_graph`
    /// cannot produce one — which is exactly why the assertion needs the direct
    /// insert to be worth anything. `graph.assert_edge` resolves endpoints from
    /// client-supplied symbol keys, so "no writer can create this row" is a
    /// property of today's writers, not of the schema; the read must not depend
    /// on it.
    #[tokio::test]
    async fn an_edge_reaching_out_of_the_repository_is_not_returned() {
        let data = tempfile::tempdir().expect("tempdir");
        let pool = pool(&data).await;
        let mine = tempfile::tempdir().expect("tempdir");
        let theirs = tempfile::tempdir().expect("tempdir");
        init_repo(mine.path());
        init_repo(theirs.path());
        let revision = GitRevision("r1".to_string());
        for (dir, source) in [
            (mine.path(), "pub fn a() {}\n"),
            (theirs.path(), "pub fn hidden() {}\n"),
        ] {
            codegraph::upsert_file_graph(
                &pool,
                scan::repository_id_for(dir),
                &revision,
                "src/lib.rs",
                source,
            )
            .await
            .expect("fold");
        }
        let mine_id = scan::repository_id_for(mine.path());
        let theirs_id = scan::repository_id_for(theirs.path());
        let from: String = sqlx::query_scalar(
            "SELECT id FROM code_nodes WHERE repository = ? AND qualified_name = 'a'",
        )
        .bind(mine_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("my node");
        let to: String = sqlx::query_scalar(
            "SELECT id FROM code_nodes WHERE repository = ? AND qualified_name = 'hidden'",
        )
        .bind(theirs_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("their node");
        sqlx::query(
            "INSERT INTO code_edges (id, from_node, to_node, relation, confidence, \
             evidence_kind, evidence_artifact, revision, created_at) \
             VALUES ('cross', ?, ?, 'calls', 0.45, 'syntax_inferred', NULL, 'r1', '2026-08-14')",
        )
        .bind(&from)
        .bind(&to)
        .execute(&pool)
        .await
        .expect("insert a cross-repository edge");

        let ops = ops(pool);
        for connected_view in [false, true] {
            let page = ops
                .read(CodeGraphReadRequest {
                    repository: mine.path().display().to_string(),
                    query: CodeGraphQuery {
                        connected_view,
                        include_edges: true,
                        include_nodes: true,
                        ..CodeGraphQuery::default()
                    },
                })
                .await
                .expect("read");
            assert!(
                !page.edges.iter().any(
                    |edge| edge.to_name.contains("hidden") || edge.from_name.contains("hidden")
                ),
                "an edge must never name a node outside this repository: {page:?}"
            );
            assert!(!page
                .nodes
                .iter()
                .any(|node| node.qualified_name == "hidden"));
            assert_eq!(page.total_edges, 2);
        }
    }

    /// A build on a checkout holding nothing the extractor can parse must still
    /// come back with the numbers that explain it — files walked, none folded,
    /// the extensions responsible, and the roster that would have worked. This
    /// is the reporter's exact case, and the assertion that a bare `0` is not
    /// an acceptable answer to it.
    #[tokio::test]
    async fn a_build_with_no_foldable_files_reports_what_it_walked() {
        let repo = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path());
        std::fs::create_dir_all(repo.path().join("app")).expect("mkdir");
        std::fs::write(repo.path().join("app/main.go"), "package main\n").expect("write");
        std::fs::write(repo.path().join("app/util.go"), "package main\n").expect("write");
        std::fs::write(repo.path().join("README"), "hello\n").expect("write");

        let data = tempfile::tempdir().expect("tempdir");
        let report = ops(pool(&data).await)
            .build(BuildCodeGraphRequest {
                repository: repo.path().display().to_string(),
            })
            .await
            .expect("an unfoldable repository is not an error");

        assert_eq!(report.nodes, 0);
        assert_eq!(report.files_folded, 0);
        assert_eq!(report.files_supported, 0);
        assert!(report.files_walked >= 3, "{report:?}");
        assert!(report.files_unsupported >= 3, "{report:?}");
        let go = report
            .not_folded
            .iter()
            .find(|row| row.extension == "go")
            .unwrap_or_else(|| panic!("the .go files must be named: {report:?}"));
        assert_eq!(go.files, 2);
        assert!(
            report.grammars.iter().any(|g| g.language == "python"),
            "the report must say what WOULD have folded: {report:?}"
        );
    }

    /// **A build immediately followed by a status must report `current`.**
    ///
    /// The full scan reads the WORKING TREE but used to stamp what it folded
    /// with the bare `HEAD` commit. `graph status` then saw a dirty tree and a
    /// graph carrying no `+workdir` stamp anywhere, and told the user — and the
    /// model — that the graph it had just built was stale, with a remedy of
    /// "run `codypendent graph build`" they had run one second earlier
    /// (2026-08-13 review, codegraph F6).
    #[tokio::test]
    async fn a_build_of_a_dirty_tree_is_not_reported_stale() {
        let repo = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path());
        std::fs::create_dir_all(repo.path().join("src")).expect("mkdir");
        std::fs::write(repo.path().join("src/lib.rs"), "pub fn committed() {}\n").expect("write");
        for args in [vec!["add", "."], vec!["commit", "-qm", "seed"]] {
            assert!(Command::new("git")
                .current_dir(repo.path())
                .args(&args)
                .status()
                .expect("run git")
                .success());
        }
        // An uncommitted edit — the state a developer is in essentially always.
        std::fs::write(
            repo.path().join("src/lib.rs"),
            "pub fn committed() {}\npub fn uncommitted_symbol() {}\n",
        )
        .expect("write");

        let data = tempfile::tempdir().expect("tempdir");
        let ops = ops(pool(&data).await);
        let report = ops
            .build(BuildCodeGraphRequest {
                repository: repo.path().display().to_string(),
            })
            .await
            .expect("build");
        assert!(
            report.revision.ends_with("+workdir"),
            "a fold of an uncommitted tree must say so: {}",
            report.revision
        );

        let status = ops
            .status(CodeGraphStatusRequest {
                repository: repo.path().display().to_string(),
            })
            .await
            .expect("status");
        assert!(status.working_tree_dirty, "{status:?}");
        assert!(
            !status.stale,
            "a graph built one call ago is not stale: {:?}",
            status.stale_reason
        );
        assert_eq!(status.stale_reason, None);
        // The stamp is the commit plus the marker, not some third thing.
        assert_eq!(
            status
                .revisions
                .iter()
                .map(|tally| tally.label.as_str())
                .collect::<Vec<_>>(),
            vec![format!("{}+workdir", status.head_revision).as_str()],
        );
    }

    /// **Reverting the edits a graph was built from makes it stale.**
    ///
    /// A build of a dirty tree stamps `<HEAD>+workdir` — rows folded from bytes
    /// that were never committed. Revert the edits and those bytes are gone, but
    /// `HEAD` never moved, so the staleness filter still accepted the stamp as
    /// "current" and the dirty-tree clause below it never ran: `graph status`
    /// reported a graph of vanished source as current, and the model's symbol
    /// map named functions the file no longer contains.
    ///
    /// Reachable without any test scaffolding: `build` treats a failure to arm
    /// the live watcher as a successful build, so nothing is guaranteed to
    /// refold the file when the edit goes away.
    #[tokio::test]
    async fn reverting_the_edits_a_graph_was_built_from_makes_it_stale() {
        let repo = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path());
        std::fs::create_dir_all(repo.path().join("src")).expect("mkdir");
        let file = repo.path().join("src/lib.rs");
        std::fs::write(&file, "pub fn committed() {}\n").expect("write");
        for args in [vec!["add", "."], vec!["commit", "-qm", "seed"]] {
            assert!(Command::new("git")
                .current_dir(repo.path())
                .args(&args)
                .status()
                .expect("run git")
                .success());
        }
        std::fs::write(&file, "pub fn committed() {}\npub fn scratch_symbol() {}\n")
            .expect("write");

        let data = tempfile::tempdir().expect("tempdir");
        let ops = ops(pool(&data).await);
        let report = ops
            .build(BuildCodeGraphRequest {
                repository: repo.path().display().to_string(),
            })
            .await
            .expect("build");
        assert!(report.revision.ends_with("+workdir"), "{}", report.revision);

        // The user throws the experiment away. Same commit, clean tree — and a
        // graph still describing `scratch_symbol`, which no file now defines.
        std::fs::write(&file, "pub fn committed() {}\n").expect("revert");

        let status = ops
            .status(CodeGraphStatusRequest {
                repository: repo.path().display().to_string(),
            })
            .await
            .expect("status");
        assert!(!status.working_tree_dirty, "{status:?}");
        assert!(
            status.stale,
            "a graph folded from reverted bytes is not current: {status:?}"
        );
        let reason = status.stale_reason.clone().expect("a reason");
        assert!(reason.contains("uncommitted"), "{reason}");
        assert!(reason.contains("graph build"), "{reason}");
    }

    /// The other half: a CLEAN tree is stamped with the bare commit, so the
    /// revision the graph reports is the one a user can `git show`. Stamping
    /// `+workdir` unconditionally would be the same lie pointing the other way.
    #[tokio::test]
    async fn a_build_of_a_clean_tree_is_stamped_with_the_commit() {
        let repo = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path());
        std::fs::create_dir_all(repo.path().join("src")).expect("mkdir");
        std::fs::write(repo.path().join("src/lib.rs"), "pub fn committed() {}\n").expect("write");
        for args in [vec!["add", "."], vec!["commit", "-qm", "seed"]] {
            assert!(Command::new("git")
                .current_dir(repo.path())
                .args(&args)
                .status()
                .expect("run git")
                .success());
        }

        let data = tempfile::tempdir().expect("tempdir");
        let ops = ops(pool(&data).await);
        let report = ops
            .build(BuildCodeGraphRequest {
                repository: repo.path().display().to_string(),
            })
            .await
            .expect("build");
        let status = ops
            .status(CodeGraphStatusRequest {
                repository: repo.path().display().to_string(),
            })
            .await
            .expect("status");
        assert!(!status.working_tree_dirty, "{status:?}");
        assert_eq!(report.revision, status.head_revision);
        assert!(!status.stale, "{:?}", status.stale_reason);
    }

    /// A repository the extractor does cover folds, and the report attributes
    /// the result per language. The mixed case the user actually reported:
    /// before the extractor widened, only `lib.rs` contributed and the Python
    /// and TSX files vanished silently.
    #[tokio::test]
    async fn a_mixed_repository_folds_every_covered_language() {
        let repo = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path());
        std::fs::create_dir_all(repo.path().join("src")).expect("mkdir");
        std::fs::write(repo.path().join("src/lib.rs"), "pub fn one() {}\n").expect("write");
        std::fs::write(repo.path().join("src/app.py"), "def two():\n    pass\n").expect("write");
        std::fs::write(
            repo.path().join("src/ui.tsx"),
            "export function Three() { return <div/>; }\n",
        )
        .expect("write");
        std::fs::write(repo.path().join("notes.go"), "package main\n").expect("write");

        let data = tempfile::tempdir().expect("tempdir");
        let report = ops(pool(&data).await)
            .build(BuildCodeGraphRequest {
                repository: repo.path().display().to_string(),
            })
            .await
            .expect("build");

        assert_eq!(report.files_folded, 3, "{report:?}");
        let languages: Vec<&str> = report
            .by_language
            .iter()
            .map(|count| count.language.as_str())
            .collect();
        for expected in ["rust", "python", "tsx"] {
            assert!(
                languages.contains(&expected),
                "{expected} must be attributed: {report:?}"
            );
        }
        assert!(report.nodes > 0 && report.edges > 0, "{report:?}");
        assert_eq!(
            report
                .not_folded
                .iter()
                .find(|row| row.extension == "go")
                .map(|row| row.files),
            Some(1),
            "{report:?}"
        );
    }
}
