//! The syntax-layer code graph (Chapter 07, STEP 2.5).
//!
//! Tree-sitter parses a source file into the durable graph: the important
//! symbols (files, modules, types, traits, functions, methods, constants, and
//! tests — never local variables) as [`CodeNode`]s keyed by a position-stable
//! [`SymbolKey`], and the `Contains` / `Defines` / `Imports` / `Calls`-as-written
//! relations between them as evidence-backed [`CodeEdge`]s.
//!
//! Which files those are is decided in exactly one place — [`language_for`] —
//! consulted both by the parser here and by the daemon's scanner. When they were
//! two independent lists the scanner's was `*.rs` only, so every Python or
//! TypeScript repository produced an empty graph and nothing said so.
//!
//! Only the *syntax* layer lives here (semantic/LSP resolution is Phase 4), so a
//! call edge is recorded "as written" — resolved to a local definition when the
//! written name matches one in the same file, otherwise pointed at a synthesized
//! [`CodeNodeKind::ExternalDependency`] node — and carries the Chapter 07
//! confidence of [`SYNTAX_CALL_CONFIDENCE`] with [`EvidenceKind::SyntaxInferred`].
//!
//! Persistence mirrors the house conventions ([`crate::outbox`],
//! `daemon::artifacts`): a stateless free function takes `pool: &SqlitePool`,
//! (de)serializes rows by binding columns, and does every write inside a single
//! `pool.begin()` transaction that also appends the index-outbox rows so the
//! authoritative write and its `SymbolChanged` events are atomic.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;

use chrono::Utc;
use codypendent_protocol::{ArtifactId, ArtifactRef, CodeNodeId, DataClassification, RepositoryId};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tree_sitter::{Node, Parser};
use uuid::Uuid;

use crate::outbox::{self, KnowledgeIndexEvent};
use crate::types::{
    CodeEdge, CodeNode, CodeNodeKind, CodeRelation, ContentHash, EvidenceKind, EvidenceRef,
    GitRevision, LanguageId, SymbolKey, AGENT_ASSERTED_CONFIDENCE, SYNTAX_CALL_CONFIDENCE,
};

mod import_resolution;
mod traversal;
pub use traversal::{
    GraphDirection, GraphTraversal, GraphTraversalKind, GraphTraversalMetadata,
    TRAVERSAL_DEFAULT_LIMIT, TRAVERSAL_MAX_LIMIT, TRAVERSAL_MAX_OFFSET,
};

// --------------------------------------------------------------------------
// Language dispatch — the single definition of "a file the graph can hold"
// --------------------------------------------------------------------------

/// A source language the syntax layer can parse into the graph.
///
/// **This enum, and [`language_for`], are the only place the supported set is
/// written down.** The daemon's warm-up walk, its filesystem watcher, and
/// [`build_file_graph`] all ask here; a second list would drift, and drift is
/// exactly how a mixed repository came to fold only its `.rs` files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    /// TypeScript with JSX. A separate grammar, not a TypeScript variant: the
    /// TypeScript grammar cannot parse `<div/>` and silently yields an error
    /// tree, which would look exactly like an empty file.
    Tsx,
    JavaScript,
}

impl Language {
    /// Every supported language, in a stable order (rendering, `--help` text).
    pub const ALL: [Language; 5] = [
        Language::Rust,
        Language::Python,
        Language::TypeScript,
        Language::Tsx,
        Language::JavaScript,
    ];

    /// The stable identifier stored in `code_nodes.language`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::JavaScript => "javascript",
        }
    }

    #[must_use]
    pub fn id(self) -> LanguageId {
        LanguageId(self.as_str().to_owned())
    }

    /// The file extensions (no leading dot, lower case) that select this
    /// language. Matched case-insensitively by [`language_for`].
    #[must_use]
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["rs"],
            Language::Python => &["py", "pyi"],
            Language::TypeScript => &["ts", "mts", "cts"],
            Language::Tsx => &["tsx"],
            Language::JavaScript => &["js", "jsx", "mjs", "cjs"],
        }
    }

    /// The media type recorded on a file's descriptive evidence artifact.
    #[must_use]
    pub fn media_type(self) -> &'static str {
        match self {
            Language::Rust => "text/x-rust",
            Language::Python => "text/x-python",
            Language::TypeScript => "text/x-typescript",
            Language::Tsx => "text/x-tsx",
            Language::JavaScript => "text/javascript",
        }
    }

    /// The separator this language's qualified names are built with. Rust nests
    /// with `::`, everything else here with `.`; the two never occur together in
    /// one name, which is what lets [`last_segment`]/[`module_of`] accept both
    /// without being told the language.
    #[must_use]
    pub fn separator(self) -> &'static str {
        match self {
            Language::Rust => "::",
            _ => ".",
        }
    }

    fn grammar(self) -> tree_sitter::Language {
        match self {
            Language::Rust => tree_sitter_rust::LANGUAGE.into(),
            Language::Python => tree_sitter_python::LANGUAGE.into(),
            Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        }
    }
}

/// The language `path`'s extension selects, or `None` when no grammar handles it.
///
/// The one gate. A caller that wants "is this file worth reading" and a caller
/// that wants "can I parse this" must be the same question, or the first will
/// happily hand the second a file it drops on the floor.
#[must_use]
pub fn language_for(path: &Path) -> Option<Language> {
    let extension = path.extension()?.to_str()?;
    Language::ALL.into_iter().find(|language| {
        language
            .extensions()
            .iter()
            .any(|candidate| extension.eq_ignore_ascii_case(candidate))
    })
}

/// Every supported extension, for an error message or a `--help` line.
#[must_use]
pub fn supported_extensions() -> Vec<&'static str> {
    Language::ALL
        .into_iter()
        .flat_map(Language::extensions)
        .copied()
        .collect()
}

/// Derive a **stable** [`RepositoryId`] from a repository's canonical path.
///
/// The daemon must map the same checkout to the same id across restarts: a fresh
/// random id per boot would orphan the previous run's `code_nodes`/`code_edges`
/// and any repository-scoped memories or skills (they become unreachable) and
/// grow the database without bound. Deterministic — the first 16 bytes of the
/// SHA-256 of the canonical path, as a UUID — so no persisted mapping is needed.
#[must_use]
pub fn stable_repository_id(canonical_path: &Path) -> RepositoryId {
    let digest = Sha256::digest(canonical_path.to_string_lossy().as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    RepositoryId(Uuid::from_bytes(bytes))
}

// --------------------------------------------------------------------------
// Full rebuild — refold every file in place, retire only what actually vanished
// --------------------------------------------------------------------------

/// The per-file counts one rebuild folded, in input order.
///
/// Deliberately not the whole [`GraphDelta`]: a 2000-file rebuild would
/// otherwise hold every node and edge record it wrote in memory purely to count
/// them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FoldedFile {
    pub nodes: usize,
    pub edges: usize,
}

/// The files a rebuild retired because the scan no longer saw them at all —
/// deleted, renamed away, newly `.gitignore`d, or past the walk's file cap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetiredFiles {
    pub files: u64,
    pub nodes: u64,
}

/// How many edges the parser cannot reproduce the repository held on either side
/// of a rebuild.
///
/// The whole point of the rebuild's shape. `after < before` means an endpoint
/// symbol genuinely disappeared with its file or its signature — the one
/// legitimate way to lose an assertion. It must never mean "a rebuild ran".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CarriedEdges {
    pub before: u64,
    pub after: u64,
}

/// Whether the file list handed to [`rebuild_repository`] is the WHOLE
/// repository, or only as much of it as a bounded walk reached.
///
/// The retire pass answers "which stored paths did this scan not see?" with
/// "delete them". That inference is only sound when the scan *finished*: a walk
/// stopped by its file cap has proven nothing about the paths it never reached,
/// and retiring them turns one truncated scan into a wiped graph. That is not
/// hypothetical — it is the reported defect: with JavaScript and TypeScript
/// folded, a `node_modules/` sorting before `src/` spent the entire cap, and the
/// build that followed retired every real file in the repository.
///
/// So the caller must state which it has, and only [`Self::Complete`] retires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanCoverage {
    /// The walk reached every in-scope path. A stored path this scan did not
    /// fold is genuinely gone, and is retired.
    Complete,
    /// The walk stopped early. Files it did reach are folded; nothing is
    /// retired, so a graph built from a complete earlier scan keeps the entries
    /// this one could not confirm. The cost is a file deleted from an
    /// over-cap repository lingering until a complete scan runs — strictly
    /// better than deleting a valid graph on the strength of a walk that never
    /// looked.
    Truncated,
}

/// What [`rebuild_repository`] wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryRebuild {
    /// One entry per input file, in input order.
    pub folded: Vec<FoldedFile>,
    /// Paths the scan did not see, and the symbols retired with them.
    pub retired: RetiredFiles,
    /// Non-syntax edge counts either side of the rebuild.
    pub edges: CarriedEdges,
}

/// Rebuild a repository's whole code graph from `files` — the authoritative fold
/// a `codypendent graph build`, a startup warm-up, or a branch switch performs.
///
/// # Why this does not clear the repository first
///
/// It used to. A bare public `clear_repository` deleted every `code_edges` and
/// `code_nodes` row for the repository with no evidence filter, and the caller
/// then refolded file by file. Two defects followed from that one line:
///
/// * **every agent assertion was destroyed on every build.** An
///   [`EvidenceKind::AgentAsserted`] edge, its confidence, and the rationale and
///   run id in its [`EvidenceRef::AgentAssertion`] all went with the wipe — and
///   the parser cannot reproduce them *by construction*, which is the entire
///   point of the feature. The incremental reparse in [`upsert_file_graph`] had
///   already been scoped to `syntax_inferred` for exactly this reason; the
///   full-rebuild path was the same class one level up.
/// * **the graph was empty for the length of the scan.** Nothing serializes the
///   readers against the writer — `graph show`, `graph status` and the agent's
///   own `graph.*` questions take no lock — so a concurrent reader saw an empty
///   or half-rebuilt repository and reported it as an answer about the user's
///   code.
///
/// Both go away by not clearing. [`upsert_file_graph`] is already authoritative
/// for its own file: it upserts by `(repository, symbol_key)` so a re-seen symbol
/// **keeps its node id**, replaces only that file's syntax edges, and retires the
/// symbols the file no longer defines. Node identity surviving is what carries
/// the layers above the parser across a rebuild — no capture-and-restore dance,
/// and nothing to lose if the scan fails halfway.
///
/// What the clear was actually needed for is a file that vanished outright:
/// nothing reparses a missing path, so its symbols would linger in the graph (and
/// in the repository map, which reads every node for the repository). That is a
/// bounded, targeted pass here — [`remove_file_graph`] for each stored
/// `source_path` this scan did not fold — instead of a repository-wide wipe.
/// It runs only for a [`ScanCoverage::Complete`] scan: "I did not see it" means
/// "it is gone" only when the walk actually finished.
///
/// The residual inconsistency is now a reader seeing *some* files at the new
/// revision and the rest at the previous one, never a truncated graph. Making the
/// whole rebuild one transaction would remove even that, at the cost of holding
/// SQLite's single write lock for the length of the scan — past the daemon's
/// 5-second `busy_timeout`, so every unrelated write in the process (the run
/// event ledger, the outbox, artifacts) would start failing with `database is
/// locked`. That trade is not worth it.
pub async fn rebuild_repository<'a, I>(
    pool: &SqlitePool,
    repository: RepositoryId,
    revision: &GitRevision,
    files: I,
    coverage: ScanCoverage,
) -> Result<RepositoryRebuild, CodeGraphError>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let files: Vec<(&str, &str)> = files.into_iter().collect();
    let before = non_syntax_edge_count(pool, repository).await?;

    // 1. Fold every file in place. Each call is its own transaction and is
    //    authoritative for its own path, so the graph is complete throughout —
    //    a mixture of this scan's rows and the previous one's, never a gap.
    let mut folded = Vec::with_capacity(files.len());
    let mut scanned: HashSet<&str> = HashSet::with_capacity(files.len());
    for (path, source) in &files {
        let delta =
            upsert_file_graph_inner(pool, repository, revision, path, source, false).await?;
        folded.push(FoldedFile {
            nodes: delta.nodes.len(),
            edges: delta.edges.len(),
        });
        scanned.insert(*path);
    }

    // 2. Retire the paths the scan did not see. This is the only job the wipe
    //    was ever doing that a per-file fold cannot: nothing reparses a file that
    //    is no longer there. Skipped outright for a truncated walk, which did not
    //    look at those paths and therefore cannot say they are gone.
    let mut retired = RetiredFiles::default();
    if coverage == ScanCoverage::Truncated {
        for (path, source) in &files {
            import_resolution::reconcile_file(pool, repository, path, source).await?;
        }
        let after = non_syntax_edge_count(pool, repository).await?;
        return Ok(RepositoryRebuild {
            folded,
            retired,
            edges: CarriedEdges { before, after },
        });
    }
    for path in stored_source_paths(pool, repository).await? {
        match path {
            Some(path) if scanned.contains(path.as_str()) => {}
            Some(path) => {
                let nodes = remove_file_graph(pool, repository, &path).await?;
                if nodes > 0 {
                    retired.files += 1;
                    retired.nodes += nodes;
                }
            }
            // Rows written before `source_path` existed (migration 0004) carry
            // NULL, and no fold can ever re-see them, so they are retired too —
            // the wipe used to take them and nothing else would.
            None => retired.nodes += remove_pathless_nodes(pool, repository).await?,
        }
    }

    // Resolve only after all definitions and retirements are visible, so file
    // ordering cannot hide a target or bind a call to a deleted file.
    for (path, source) in &files {
        import_resolution::reconcile_file(pool, repository, path, source).await?;
    }
    let after = non_syntax_edge_count(pool, repository).await?;
    Ok(RepositoryRebuild {
        folded,
        retired,
        edges: CarriedEdges { before, after },
    })
}

/// How many edges in `repository` no parser could have produced — everything
/// whose evidence is not [`EvidenceKind::SyntaxInferred`].
async fn non_syntax_edge_count(
    pool: &SqlitePool,
    repository: RepositoryId,
) -> Result<u64, CodeGraphError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM code_edges e JOIN code_nodes n ON e.from_node = n.id \
         WHERE n.repository = ? AND e.evidence_kind <> ?",
    )
    .bind(repository.to_string())
    .bind(scalar(&EvidenceKind::SyntaxInferred))
    .fetch_one(pool)
    .await?;
    Ok(count.max(0) as u64)
}

/// Every distinct `source_path` the repository currently has nodes under, `None`
/// for the pre-migration rows that have none.
async fn stored_source_paths(
    pool: &SqlitePool,
    repository: RepositoryId,
) -> Result<Vec<Option<String>>, CodeGraphError> {
    Ok(
        sqlx::query_scalar("SELECT DISTINCT source_path FROM code_nodes WHERE repository = ?")
            .bind(repository.to_string())
            .fetch_all(pool)
            .await?,
    )
}

/// Retire the repository's nodes that carry no `source_path` at all, in one
/// transaction. The [`remove_file_graph`] of the pre-migration rows: `= NULL`
/// matches nothing in SQL, so they need their own predicate or they linger
/// forever once the repository-wide wipe is gone.
async fn remove_pathless_nodes(
    pool: &SqlitePool,
    repository: RepositoryId,
) -> Result<u64, CodeGraphError> {
    let repo = repository.to_string();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "DELETE FROM code_edges WHERE from_node IN \
         (SELECT id FROM code_nodes WHERE repository = ? AND source_path IS NULL) \
         OR to_node IN \
         (SELECT id FROM code_nodes WHERE repository = ? AND source_path IS NULL)",
    )
    .bind(&repo)
    .bind(&repo)
    .execute(&mut *tx)
    .await?;
    let removed =
        sqlx::query("DELETE FROM code_nodes WHERE repository = ? AND source_path IS NULL")
            .bind(&repo)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    tx.commit().await?;
    Ok(removed)
}

/// Errors from parsing or persisting the code graph.
#[derive(Debug, thiserror::Error)]
pub enum CodeGraphError {
    /// A SQLite / sqlx failure.
    #[error("sqlite error: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// A JSON (de)serialization failure for a stored scalar or evidence blob.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// A stored id column did not parse back into its UUID newtype.
    #[error("invalid id: {0}")]
    Id(#[from] uuid::Error),
    /// The tree-sitter parser could not be configured or produced no tree.
    #[error("parse error: {0}")]
    Parse(String),
    /// No grammar handles this file's extension. Explicit rather than a silent
    /// empty graph: a caller that reaches here has a filter that disagrees with
    /// [`language_for`], which is the bug this variant exists to expose.
    #[error("unsupported language for {path} (supported extensions: {supported})", supported = supported_extensions().join(", "))]
    UnsupportedLanguage { path: String },
    /// A filesystem watcher could not be created or armed.
    #[error("watch error: {0}")]
    Watch(#[from] notify::Error),
}

/// The result of (re)parsing one file into the graph, returned by
/// [`upsert_file_graph`]. The `nodes`/`edges` are the full graph *for this
/// file*; because the parse is deterministic, an incremental single-file reparse
/// yields the same sets as a full reparse of that file (the STEP 2.5 property).
#[derive(Debug, Clone, PartialEq)]
pub struct GraphDelta {
    /// The repo-relative path that was parsed.
    pub path: String,
    /// The revision every node/edge in this delta was stamped with.
    pub revision: GitRevision,
    /// Every node upserted for this file (durable symbols plus the synthesized
    /// import/call reference nodes the edges point at).
    pub nodes: Vec<CodeNode>,
    /// Every edge (re)written for this file.
    pub edges: Vec<CodeEdge>,
    /// The subset of `nodes` that were newly inserted on this call (as opposed to
    /// re-seen, which only bumps their revision and keeps their id).
    pub created_node_ids: Vec<CodeNodeId>,
    /// How many stale edges from the previous parse of this file were removed.
    pub removed_edges: u64,
}

// --------------------------------------------------------------------------
// Scan reporting — what a warm-up actually did, so a caller can say so
// --------------------------------------------------------------------------

/// What one repository scan saw and folded.
///
/// A scan used to return `()`. On a repository in a language the graph could not
/// parse it walked thousands of files, folded none, and reported success — the
/// graph was empty and *nothing said so*. Every field here exists so a caller
/// (`codypendent graph status`, a daemon log line) can explain the size of the
/// graph instead of presenting an empty one as a finished one.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScanSummary {
    /// Every regular file the walk actually visited, before any per-file filter.
    /// The denominator.
    ///
    /// A file inside a pruned directory is **not** counted here, because the
    /// walk never went in — see [`Self::dirs_pruned`]. That is the point of
    /// pruning: an excluded dependency tree must cost the scan nothing, neither
    /// a `read_dir` nor a slot under the file cap.
    pub files_seen: usize,
    /// Of those, the ones [`language_for`] recognises — the fold candidates.
    pub files_supported: usize,
    /// Of those, the ones actually folded into the graph.
    pub files_folded: usize,
    /// Candidates dropped by `.gitignore`.
    pub files_skipped_ignored: usize,
    /// Directories the walk refused to descend into: the checkout's ignore rules
    /// excluded them, or they are the build/VCS/dependency trees no walk enters.
    /// Their contents are absent from every other count here.
    pub dirs_pruned: usize,
    /// Candidates that vanished between the walk and the read. Counted apart from
    /// `files_skipped_ignored` because "the tree moved under us" and "policy
    /// excluded it" are different answers to "why is this file not in the graph".
    pub files_skipped_unreadable: usize,
    /// Files no grammar handles. The number that explains an empty graph.
    pub files_skipped_unsupported: usize,
    /// Folded file count per [`Language::as_str`], ordered for stable rendering.
    pub folded_by_language: BTreeMap<String, usize>,
    /// Unsupported extensions actually seen, with counts (`{"go": 1204}`), so a
    /// report can name what it skipped. Bounded to
    /// [`Self::MAX_TRACKED_EXTENSIONS`] distinct keys — a hostile or generated
    /// tree can contain unboundedly many distinct extensions, and this map is
    /// held in memory and rendered.
    pub unsupported_by_extension: BTreeMap<String, usize>,
    /// Nodes and edges written by this scan.
    pub nodes: usize,
    pub edges: usize,
    /// The revision every node and edge this scan wrote was **stamped** with.
    ///
    /// Reported rather than re-derived by the caller: a scan reads the *working
    /// tree*, so on a dirty checkout what it folded is not what `HEAD` holds, and
    /// a report that asked Git a second time could name a different answer than
    /// the rows carry (2026-08-13 review, codegraph F6).
    pub revision: String,
    /// Non-syntax edge counts (agent-, LSP-, compiler-asserted) either side of
    /// the rebuild. A shortfall means a symbol an assertion named genuinely
    /// vanished; a rebuild on its own must never move these numbers.
    pub carried_edges: CarriedEdges,
    /// Paths the scan no longer saw, and the symbols retired with them.
    pub retired: RetiredFiles,
    /// The walk stopped at [`Self::file_cap`]: this graph is a *truncation* of
    /// the repository, not the repository.
    ///
    /// Also set when the SYMBOL budget stopped the fold (see
    /// [`Self::truncated_by_node_budget`] and [`Self::files_skipped_oversized`]),
    /// because it means the same thing to the one caller that acts on it: this
    /// scan did not look at every in-scope path, so it may not retire anything.
    pub truncated_by_cap: bool,
    /// The file cap that was in force.
    pub file_cap: usize,
    /// The whole-scan node budget that was in force, and the per-file node cap.
    /// Zero when the caller ran an unbudgeted walk.
    #[serde(default)]
    pub node_budget: usize,
    #[serde(default)]
    pub file_node_cap: usize,
    /// Files dropped for folding to more than [`Self::file_node_cap`] symbols on
    /// their own — machine-generated bindings, minified bundles, generated
    /// protobuf. Counted apart from every other skip: nothing is wrong with the
    /// file, it is simply too heavy to be worth a fifth of the graph.
    #[serde(default)]
    pub files_skipped_oversized: usize,
    /// The whole-scan node budget ran out before the file list did.
    #[serde(default)]
    pub truncated_by_node_budget: bool,
    /// The heaviest files this scan measured, node count descending — the
    /// diagnostic that explains an enormous graph.
    ///
    /// Recorded whenever a budget bit, so the answer to "why did this repository
    /// produce half a million nodes" is in the scan's own report instead of
    /// something to be inferred from a 1.9 GB database after the fact. Bounded to
    /// [`Self::MAX_HEAVIEST_FILES`].
    #[serde(default)]
    pub heaviest_files: Vec<FileNodeWeight>,
}

/// One file's measured contribution to the graph, for [`ScanSummary::heaviest_files`].
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileNodeWeight {
    /// The repo-relative path.
    pub path: String,
    /// How many nodes folding it would write.
    pub nodes: usize,
}

impl ScanSummary {
    /// How many distinct unsupported extensions are tracked by name.
    pub const MAX_TRACKED_EXTENSIONS: usize = 32;

    /// How many entries [`Self::heaviest_files`] keeps. Enough to name the
    /// offending tree, short enough to print in a log line.
    pub const MAX_HEAVIEST_FILES: usize = 5;

    /// Record the measured node weights of the files this scan considered,
    /// keeping the [`Self::MAX_HEAVIEST_FILES`] heaviest.
    ///
    /// Ordered node count descending, then path ascending, so two scans of one
    /// tree render identically instead of reordering ties.
    pub fn record_heaviest(&mut self, mut weights: Vec<FileNodeWeight>) {
        weights.sort_by(|a, b| b.nodes.cmp(&a.nodes).then_with(|| a.path.cmp(&b.path)));
        weights.truncate(Self::MAX_HEAVIEST_FILES);
        self.heaviest_files = weights;
    }

    /// Count one unsupported file, tracking its extension while there is room.
    pub fn record_unsupported(&mut self, path: &Path) {
        self.files_skipped_unsupported += 1;
        let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
            return;
        };
        let extension = extension.to_ascii_lowercase();
        if self.unsupported_by_extension.len() < Self::MAX_TRACKED_EXTENSIONS
            || self.unsupported_by_extension.contains_key(&extension)
        {
            *self.unsupported_by_extension.entry(extension).or_default() += 1;
        }
    }

    /// Count one folded file against its language.
    pub fn record_folded(&mut self, language: Language) {
        self.files_folded += 1;
        *self
            .folded_by_language
            .entry(language.as_str().to_owned())
            .or_default() += 1;
    }

    /// The walk found files and folded none of them — the failure this whole
    /// type exists to make loud. A caller should warn, not print a zero.
    #[must_use]
    pub fn found_nothing_to_fold(&self) -> bool {
        self.files_folded == 0 && self.files_seen > 0
    }

    /// One line a CLI or a log can print verbatim.
    #[must_use]
    pub fn headline(&self) -> String {
        let mut line = format!(
            "folded {} of {} files seen ({} nodes, {} edges)",
            self.files_folded, self.files_seen, self.nodes, self.edges
        );
        if !self.folded_by_language.is_empty() {
            let langs: Vec<String> = self
                .folded_by_language
                .iter()
                .map(|(name, count)| format!("{name} {count}"))
                .collect();
            line.push_str(&format!(" [{}]", langs.join(", ")));
        }
        if self.files_skipped_unsupported > 0 {
            let extensions: Vec<String> = self
                .unsupported_by_extension
                .iter()
                .map(|(extension, count)| format!(".{extension} {count}"))
                .collect();
            line.push_str(&format!(
                "; {} unsupported ({})",
                self.files_skipped_unsupported,
                if extensions.is_empty() {
                    "no extension".to_owned()
                } else {
                    extensions.join(", ")
                }
            ));
        }
        if self.files_skipped_ignored > 0 {
            line.push_str(&format!("; {} ignored", self.files_skipped_ignored));
        }
        // Reported apart from `files_skipped_ignored`, and never folded into it:
        // a pruned directory stands for an unknown (often enormous) number of
        // files the walk deliberately never counted, and printing it as "1
        // ignored" would understate a `node_modules` by four orders of magnitude.
        if self.dirs_pruned > 0 {
            line.push_str(&format!("; {} director(ies) not walked", self.dirs_pruned));
        }
        if self.retired.files > 0 {
            line.push_str(&format!(
                "; retired {} vanished file(s) ({} symbols)",
                self.retired.files, self.retired.nodes
            ));
        }
        // The non-syntax layer is the part a rebuild cannot regenerate, so a
        // rebuild that dropped any of it has to say so where the fold is
        // reported — silently losing an assertion is the defect, not the number.
        if self.carried_edges.before > 0 || self.carried_edges.after > 0 {
            line.push_str(&format!(
                "; {} asserted edge(s) carried",
                self.carried_edges.after
            ));
            if self.carried_edges.after < self.carried_edges.before {
                line.push_str(&format!(
                    " of {} — {} lost to symbols that no longer exist",
                    self.carried_edges.before,
                    self.carried_edges.before - self.carried_edges.after
                ));
            }
        }
        if self.files_skipped_unreadable > 0 {
            line.push_str(&format!("; {} unreadable", self.files_skipped_unreadable));
        }
        if self.files_skipped_oversized > 0 {
            line.push_str(&format!(
                "; {} file(s) skipped over the {}-node per-file cap",
                self.files_skipped_oversized, self.file_node_cap
            ));
        }
        if self.truncated_by_node_budget {
            line.push_str(&format!(
                "; TRUNCATED at the {}-node budget — this graph is incomplete, \
                 and nothing was retired (an unfinished walk proves no file gone)",
                self.node_budget
            ));
        } else if self.truncated_by_cap {
            line.push_str(&format!(
                "; TRUNCATED at the {}-file cap — this graph is incomplete, \
                 and nothing was retired (an unfinished walk proves no file gone)",
                self.file_cap
            ));
        }
        // Named only when a budget actually bit. A scan that fitted comfortably
        // needs no explanation, and printing the five biggest files of every
        // healthy repository would bury the case that does.
        if !self.heaviest_files.is_empty() && (self.truncated_by_cap || self.files_folded == 0) {
            let heaviest: Vec<String> = self
                .heaviest_files
                .iter()
                .map(|file| format!("{} {}", file.path, file.nodes))
                .collect();
            line.push_str(&format!("; heaviest files: {}", heaviest.join(", ")));
        }
        if self.found_nothing_to_fold() {
            line.push_str(&format!(
                "; NO supported source found — the graph is empty (supported: {})",
                supported_extensions().join(", ")
            ));
        }
        line
    }
}

// --------------------------------------------------------------------------
// Public API — parse + persist
// --------------------------------------------------------------------------

/// Parse `source` (repo-relative `path`) and fold it into the graph for
/// `repository` at `revision`, in a single transaction.
///
/// Nodes are upserted by their unique `(repository, symbol_key)` — which now
/// folds in the `source_path`, so identity is scoped to the file. A re-seen
/// symbol keeps its `code_nodes.id` (identity survives line movement *within the
/// file*) and only has its `revision` bumped; a new symbol gets a fresh id.
/// The file's edges are then replaced wholesale (every edge whose `from_node` is
/// one of this file's own nodes — i.e. shares this `source_path` — is deleted and
/// reinserted), any symbol this file *no longer* defines is retired (so a
/// single-file reparse is self-sufficient; issue #6 item 4), and one
/// `SymbolChanged` outbox event is enqueued per durable node — all atomic.
pub async fn upsert_file_graph(
    pool: &SqlitePool,
    repository: RepositoryId,
    revision: &GitRevision,
    path: &str,
    source: &str,
) -> Result<GraphDelta, CodeGraphError> {
    upsert_file_graph_inner(pool, repository, revision, path, source, true).await
}

/// Importer paths to reparse after changing a source file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AffectedImporters {
    pub paths: Vec<String>,
    pub truncated: bool,
}

/// Capture importers before retiring changed definitions. Changed files are
/// excluded because their own parse already refreshes their outgoing edges.
pub async fn affected_importers(
    pool: &SqlitePool,
    repository: RepositoryId,
    changed_paths: &[String],
    limit: usize,
) -> Result<AffectedImporters, CodeGraphError> {
    affected_importers_page(pool, repository, changed_paths, None, limit).await
}

/// Read a stable, sorted page of explicit importers. Continue with the last
/// returned path as `after` while `truncated`; each page holds at most 2000 paths.
/// The caller holds the repository writer gate across capture and mutation.
pub async fn affected_importers_page(
    pool: &SqlitePool,
    repository: RepositoryId,
    changed_paths: &[String],
    after: Option<&str>,
    limit: usize,
) -> Result<AffectedImporters, CodeGraphError> {
    import_resolution::affected_importers_page(pool, repository, changed_paths, after, limit).await
}

/// Reparse one unchanged importer after its dependencies changed. The caller
/// supplies current, repository-relative source under the repository writer gate.
pub async fn reconcile_imports_for_file(
    pool: &SqlitePool,
    repository: RepositoryId,
    revision: &GitRevision,
    path: &str,
    source: &str,
) -> Result<GraphDelta, CodeGraphError> {
    upsert_file_graph(pool, repository, revision, path, source).await
}

async fn upsert_file_graph_inner(
    pool: &SqlitePool,
    repository: RepositoryId,
    revision: &GitRevision,
    path: &str,
    source: &str,
    resolve_imports: bool,
) -> Result<GraphDelta, CodeGraphError> {
    let built = build_file_graph(repository, path, source)?;
    let now = Utc::now();
    let created_at = now.to_rfc3339();

    let mut tx = pool.begin().await?;

    // 1. Upsert every node, preserving ids for re-seen symbols.
    let mut ids: Vec<CodeNodeId> = Vec::with_capacity(built.nodes.len());
    let mut created_node_ids = Vec::new();
    let mut owned_ids = Vec::new();
    let mut node_records = Vec::with_capacity(built.nodes.len());
    for node in &built.nodes {
        let symbol_key = node.key.stable_key();
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT id FROM code_nodes WHERE repository = ? AND symbol_key = ?")
                .bind(repository.to_string())
                .bind(&symbol_key)
                .fetch_optional(&mut *tx)
                .await?;
        let id = match existing {
            Some((raw,)) => {
                let id = CodeNodeId::from_str(&raw)?;
                sqlx::query("UPDATE code_nodes SET revision = ? WHERE id = ?")
                    .bind(&revision.0)
                    .bind(&raw)
                    .execute(&mut *tx)
                    .await?;
                id
            }
            None => {
                let id = CodeNodeId::new();
                sqlx::query(
                    "INSERT INTO code_nodes \
                     (id, repository, language, package, source_path, qualified_name, kind, \
                      signature_hash, symbol_key, revision, created_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(id.to_string())
                .bind(repository.to_string())
                .bind(&node.key.language.0)
                .bind(node.key.package.as_deref())
                .bind(&node.key.source_path)
                .bind(&node.key.qualified_name)
                .bind(scalar(&node.key.kind))
                .bind(node.key.signature_hash.as_ref().map(|h| h.0.as_str()))
                .bind(&symbol_key)
                .bind(&revision.0)
                .bind(&created_at)
                .execute(&mut *tx)
                .await?;
                created_node_ids.push(id);
                id
            }
        };
        ids.push(id);
        node_records.push(CodeNode {
            id,
            key: node.key.clone(),
            revision: revision.clone(),
        });
        if node.owned {
            owned_ids.push(id);
        }
    }

    // 2. Replace this file's SYNTAX edges. Every edge produced by parsing a file
    //    has a `from_node` that is one of the file's own nodes (they all carry
    //    this `source_path`), so deleting by that set removes exactly the
    //    previous parse's edges — including edges out of a symbol this reparse
    //    drops — and nothing from any other file.
    //
    //    Scoped to `syntax_inferred` because a reparse is only authoritative
    //    over the layer it produces. Unscoped, it also deleted every
    //    LSP-, compiler- and agent-asserted edge out of the file: with the live
    //    watcher armed, an agent asserts a route→service edge and the next save
    //    of that file erases it. Higher layers are retired by node retirement
    //    (2b) when their endpoint genuinely disappears, not by every save.
    let removed = sqlx::query(
        "DELETE FROM code_edges WHERE evidence_kind = ? AND from_node IN \
         (SELECT id FROM code_nodes WHERE repository = ? AND source_path = ?)",
    )
    .bind(scalar(&EvidenceKind::SyntaxInferred))
    .bind(repository.to_string())
    .bind(path)
    .execute(&mut *tx)
    .await?;
    let removed_edges = removed.rows_affected();

    // 2b. Retire any symbol this file no longer defines (issue #6 item 4). Prior
    //     nodes for this `source_path` that this parse did not re-see must first
    //     have EVERY incident edge removed, in both directions. Foreign keys are
    //     ON and `code_edges` has no `ON DELETE CASCADE`, so one surviving
    //     reference makes the node delete fail and the whole reparse error out.
    //
    //     Both directions, not just incoming: step 2 above drops only this file's
    //     *syntax* out-edges, precisely so an agent- or LSP-asserted edge survives
    //     an ordinary save. That leaves an asserted edge OUT of a symbol whose
    //     signature just changed still pointing at the node being retired — a
    //     dangling reference that failed the delete and made every later fold of
    //     the repository fail with it. An edge whose endpoint no longer exists is
    //     stale either way, in whichever direction it runs.
    if !ids.is_empty() {
        retire_absent_nodes(&mut tx, repository, path, &ids).await?;
    }

    // 3. Fold the fresh edges in, each carrying its descriptive evidence ref.
    //
    //    Through [`fold_edge`], the SAME confidence rule the semantic path uses
    //    — not a bare INSERT. Step 2 above deletes only the `syntax_inferred`
    //    layer, so an agent-asserted (0.40) or LSP (0.90) edge for a triple this
    //    reparse also emits survives that delete; inserting beside it produced a
    //    literal duplicate row, which `graph show` listed twice and
    //    `graph.callers_of` returned twice. A reparsed syntax edge supersedes a
    //    strictly weaker incumbent and yields to a stronger one, exactly as an
    //    asserted or LSP edge does.
    let imported = if resolve_imports {
        import_resolution::resolve(&mut tx, repository, path, source).await?
    } else {
        import_resolution::Resolution::default()
    };
    let mut edge_records = Vec::with_capacity(built.edges.len());
    for edge in &built.edges {
        let record = CodeEdge {
            from: ids[edge.from],
            to: if edge.relation == CodeRelation::Calls {
                imported
                    .calls
                    .get(&(edge.site_start, edge.site_end))
                    .copied()
                    .unwrap_or(ids[edge.to])
            } else if edge.relation == CodeRelation::Imports {
                imported
                    .imports
                    .get(&built.nodes[edge.to].key.qualified_name)
                    .copied()
                    .unwrap_or(ids[edge.to])
            } else {
                ids[edge.to]
            },
            relation: edge.relation,
            confidence: edge.confidence,
            evidence_kind: edge.evidence_kind,
            evidence: Some(EvidenceRef::Artifact {
                artifact: built.file_artifact.clone(),
                source_path: Some(format!("{path}#{}-{}", edge.site_start, edge.site_end)),
            }),
            revision: revision.clone(),
        };
        if let EdgeFold::Written { .. } = fold_edge(&mut tx, &record, &created_at).await? {
            edge_records.push(record);
        }
    }

    edge_records.extend(
        import_resolution::add_relations(&mut tx, repository, path, source, &imported.relations)
            .await?,
    );
    let pruned = import_resolution::prune_references(&mut tx, repository, path).await?;
    node_records.retain(|node| !pruned.contains(&node.id));
    created_node_ids.retain(|id| !pruned.contains(id));

    // 4. One SymbolChanged event per durable node, in the SAME transaction.
    for id in &owned_ids {
        outbox::enqueue(&mut *tx, &KnowledgeIndexEvent::SymbolChanged(*id), now).await?;
    }

    tx.commit().await?;

    Ok(GraphDelta {
        path: path.to_owned(),
        revision: revision.clone(),
        nodes: node_records,
        edges: edge_records,
        created_node_ids,
        removed_edges,
    })
}

/// Retire every symbol a **deleted** file defined, in one transaction.
///
/// [`upsert_file_graph`] retires the symbols a file no longer defines, but it
/// only ever runs on a file that still exists — nothing reparses a path that was
/// removed, so its nodes would linger until the next [`rebuild_repository`].
/// The incremental watcher calls this instead when a watched path
/// disappears (deleted, or renamed away), which is what makes a live per-file
/// pipeline self-sufficient without a repository-wide wipe.
///
/// Edges are removed in both directions first — the file's own outgoing edges,
/// and any edge from another file pointing INTO a symbol that is about to
/// vanish (foreign keys are ON, so a still-referenced node cannot be deleted;
/// an edge into a deleted symbol is stale regardless). Returns how many nodes
/// were retired, so a caller can report a no-op honestly.
pub async fn remove_file_graph(
    pool: &SqlitePool,
    repository: RepositoryId,
    path: &str,
) -> Result<u64, CodeGraphError> {
    let repo = repository.to_string();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "DELETE FROM code_edges WHERE from_node IN \
         (SELECT id FROM code_nodes WHERE repository = ? AND source_path = ?) \
         OR to_node IN \
         (SELECT id FROM code_nodes WHERE repository = ? AND source_path = ?)",
    )
    .bind(&repo)
    .bind(path)
    .bind(&repo)
    .bind(path)
    .execute(&mut *tx)
    .await?;
    let removed = sqlx::query("DELETE FROM code_nodes WHERE repository = ? AND source_path = ?")
        .bind(&repo)
        .bind(path)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(removed)
}

/// Parse a file graph without mutating persistence. Full-repository rebuilders
/// use this preflight so one malformed file cannot trigger a destructive clear
/// followed by a partial rebuild.
pub fn validate_file_graph(
    repository: RepositoryId,
    path: &str,
    source: &str,
) -> Result<(), CodeGraphError> {
    measure_file_graph(repository, path, source).map(|_| ())
}

/// How many nodes and edges folding this file *would* write — the same parse
/// [`validate_file_graph`] performs, with the counts kept instead of discarded.
///
/// A file-count budget cannot bound a graph, because one file's contribution is
/// unbounded: a machine-generated FFI binding module (a vendored `windows-sys`
/// `mod.rs`) folds to five figures of symbols on its own, so 2000 such files
/// under a 2000-file cap produced 386,572 nodes. The scan therefore has to know
/// the *weight* of a file before it spends its budget on it, and the only honest
/// source of that number is the parse itself.
///
/// Costs nothing extra where it is used: the full-scan preflight already parsed
/// every file once to validate it, and this is that parse.
pub fn measure_file_graph(
    repository: RepositoryId,
    path: &str,
    source: &str,
) -> Result<FoldedFile, CodeGraphError> {
    // Exactly the counts `upsert_file_graph` reports for the same bytes: it
    // upserts one row per `built.nodes` entry and returns them as the delta.
    build_file_graph(repository, path, source).map(|built| FoldedFile {
        nodes: built.nodes.len(),
        edges: built.edges.len(),
    })
}

/// Read back every node for `repository`, oldest first.
pub async fn nodes(
    pool: &SqlitePool,
    repository: RepositoryId,
) -> Result<Vec<CodeNode>, CodeGraphError> {
    let rows: Vec<NodeRow> = sqlx::query_as(
        "SELECT id, language, package, source_path, qualified_name, kind, signature_hash, \
                revision, created_at \
         FROM code_nodes WHERE repository = ? ORDER BY created_at ASC, id ASC",
    )
    .bind(repository.to_string())
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| row.into_node(repository))
        .collect()
}

/// Read back every edge for `repository` (scoped by joining `from_node` back to
/// the owning repository), oldest first.
pub async fn edges(
    pool: &SqlitePool,
    repository: RepositoryId,
) -> Result<Vec<CodeEdge>, CodeGraphError> {
    let rows: Vec<EdgeRow> = sqlx::query_as(
        "SELECT e.from_node, e.to_node, e.relation, e.confidence, e.evidence_kind, \
                e.evidence_artifact, e.revision \
         FROM code_edges e JOIN code_nodes n ON e.from_node = n.id \
         WHERE n.repository = ? ORDER BY e.created_at ASC, e.id ASC",
    )
    .bind(repository.to_string())
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(EdgeRow::into_edge).collect()
}

/// A symbol produced by a pure parse of one file (no persistence, no repository
/// id) — the shape a [`crate::adapter::LanguageAdapter`] returns from `parse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSymbol {
    pub qualified_name: String,
    pub kind: CodeNodeKind,
    pub signature_hash: Option<String>,
}

/// Parse `source` (repo-relative `path`) into the durable symbols it defines,
/// with no side effects. Reuses the exact tree-sitter walk that
/// [`upsert_file_graph`] persists, so an adapter and the graph agree on symbols.
pub fn parse_symbols(path: &str, source: &str) -> Result<Vec<ParsedSymbol>, CodeGraphError> {
    // A nil repository id: `build_file_graph` needs one to shape `SymbolKey`s, but
    // this pure parse discards the id, keeping only name/kind/signature.
    let built = build_file_graph(RepositoryId(Uuid::nil()), path, source)?;
    Ok(built
        .nodes
        .iter()
        .filter(|n| n.owned)
        .map(|n| ParsedSymbol {
            qualified_name: n.key.qualified_name.clone(),
            kind: n.key.kind,
            signature_hash: n.key.signature_hash.clone().map(|h| h.0),
        })
        .collect())
}

// --------------------------------------------------------------------------
// Semantic layer — LSP/compiler edge supersession (STEP 4.5)
// --------------------------------------------------------------------------

/// A semantic (LSP-, compiler-, or agent-asserted) edge to fold into the graph.
/// Its endpoints are named by the stable [`SymbolKey::stable_key`] rather than a
/// node id, so an adapter that resolves references does not need to know the
/// graph's internal ids. A model-callable caller wants [`assert_agent_edges`]
/// instead — it names endpoints the way the source does.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticEdge {
    pub from_symbol_key: String,
    pub to_symbol_key: String,
    pub relation: CodeRelation,
    /// [`EvidenceKind::LspResolved`], [`EvidenceKind::CompilerResolved`], or
    /// [`EvidenceKind::AgentAsserted`]. What it may supersede follows from its
    /// `confidence`, never from the kind itself.
    pub evidence_kind: EvidenceKind,
    pub confidence: f32,
    pub evidence: Option<EvidenceRef>,
}

/// What an [`upsert_semantic_edges`] / [`assert_agent_edges`] call did. Every
/// input edge lands in exactly one bucket, so the three sum to the input length.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SemanticUpsertOutcome {
    /// Written (superseding a strictly less-confident edge if one existed).
    pub applied: u64,
    /// An endpoint is not in the graph, or (for [`assert_agent_edges`]) the name
    /// matched several symbols. Never invented: an assertion cannot create nodes.
    pub skipped_unresolved: u64,
    /// An edge for that `(from, to, relation)` already exists at **equal or
    /// higher** confidence, so this one did not overwrite it.
    pub skipped_outranked: u64,
}

/// One edge the agent claims exists that no parser can see — a route handler to
/// the service it dispatches to, a config key to its reader. Endpoints are named
/// the way they appear in the source, not by the `symbol_key` composite.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentEdgeAssertion {
    pub from_symbol: String,
    pub to_symbol: String,
    pub relation: CodeRelation,
    /// Why the agent believes this edge holds, and which run said so — an
    /// [`EvidenceRef::AgentAssertion`]. Carried onto the edge so a user reviewing
    /// the graph can see the reason, not just the claim.
    pub evidence: Option<EvidenceRef>,
}

/// What one assertion did. Returned **per input assertion, in input order**: a
/// bare count cannot tell the model *which* name it got wrong, so it can only
/// resend everything and hope.
#[derive(Debug, Clone, PartialEq)]
pub enum AssertionResult {
    /// Written; nothing was displaced.
    Applied,
    /// Written; it replaced a strictly less-confident edge for the same triple.
    Superseded {
        previous: EvidenceKind,
        previous_confidence: f32,
    },
    /// **Not** written: an edge for this triple is at least as confident. Not an
    /// error — the graph already knows, from a better witness.
    Outranked {
        existing: EvidenceKind,
        existing_confidence: f32,
    },
    /// **Not** written: the name matched no symbol. `symbol` is the endpoint as
    /// the agent wrote it, so a caller can quote it back; `candidates` are near
    /// names, so a dead end becomes a correction.
    Unresolved {
        symbol: String,
        candidates: Vec<String>,
    },
    /// **Not** written: the name matched several symbols. A different next move
    /// from `Unresolved` — the agent must say which, not invent a new name.
    Ambiguous {
        symbol: String,
        candidates: Vec<String>,
    },
}

/// Fold agent-asserted edges into the graph at [`AGENT_ASSERTED_CONFIDENCE`].
///
/// Endpoints are resolved with [`find_symbols`] — the same three-tier lookup
/// `graph.callers_of` uses — so the agent names symbols as it read them. A name
/// that matches nothing, or matches more than one symbol, is refused rather than
/// guessed at, and **no node is ever created**: an assertion is a claim about
/// the graph, never an addition to it.
///
/// An assertion is refused against every mechanical layer, tree-sitter included
/// (see [`AGENT_ASSERTED_CONFIDENCE`] and [`upsert_semantic_edges`] for the rule).
pub async fn assert_agent_edges(
    pool: &SqlitePool,
    repository: RepositoryId,
    revision: &GitRevision,
    assertions: &[AgentEdgeAssertion],
) -> Result<Vec<AssertionResult>, CodeGraphError> {
    // Resolve first, so a name failure is reported against ITS assertion rather
    // than collapsing into an aggregate count.
    let mut results: Vec<Option<AssertionResult>> = Vec::with_capacity(assertions.len());
    let mut resolved: Vec<(usize, SemanticEdge)> = Vec::new();
    for assertion in assertions {
        let from = resolve_asserted_endpoint(pool, repository, &assertion.from_symbol).await?;
        let to = resolve_asserted_endpoint(pool, repository, &assertion.to_symbol).await?;
        match (from, to) {
            (Ok(from), Ok(to)) => {
                resolved.push((
                    results.len(),
                    SemanticEdge {
                        from_symbol_key: from,
                        to_symbol_key: to,
                        relation: assertion.relation,
                        evidence_kind: EvidenceKind::AgentAsserted,
                        confidence: AGENT_ASSERTED_CONFIDENCE,
                        evidence: assertion.evidence.clone(),
                    },
                ));
                results.push(None);
            }
            // The `from` failure is reported first: it is the one the agent named
            // first, and reporting both would double-count the assertion.
            (Err(failure), _) | (Ok(_), Err(failure)) => results.push(Some(failure)),
        }
    }

    let edges: Vec<SemanticEdge> = resolved.iter().map(|(_, edge)| edge.clone()).collect();
    let applied = apply_semantic_edges(pool, repository, revision, &edges).await?;
    for ((slot, _), result) in resolved.into_iter().zip(applied) {
        results[slot] = Some(result);
    }
    Ok(results
        .into_iter()
        .map(|result| result.unwrap_or(AssertionResult::Applied))
        .collect())
}

/// Resolve one endpoint name to its `symbol_key`, or to the refusal to report.
///
/// Ambiguity is a refusal, not a coin toss: an agent asserting `create` in a
/// repository with four `create`s must be told which four.
#[allow(clippy::type_complexity)]
async fn resolve_asserted_endpoint(
    pool: &SqlitePool,
    repository: RepositoryId,
    name: &str,
) -> Result<Result<String, AssertionResult>, CodeGraphError> {
    let matches = find_symbols(pool, repository, name, GRAPH_CANDIDATE_LIMIT).await?;
    match matches.as_slice() {
        [only] => Ok(Ok(only.key.stable_key())),
        [] => Ok(Err(AssertionResult::Unresolved {
            symbol: name.to_owned(),
            candidates: nearby_symbols(pool, repository, name).await?,
        })),
        several => Ok(Err(AssertionResult::Ambiguous {
            symbol: name.to_owned(),
            candidates: several
                .iter()
                .map(|node| node.key.qualified_name.clone())
                .collect(),
        })),
    }
}

/// Fold semantic edges into the graph, **superseding only an edge of strictly
/// lower confidence** for the same `(from, to, relation)`.
///
/// This used to be an unconditional `DELETE` before the insert, which was safe
/// only while every writer was an LSP or a compiler. It is not safe now:
/// [`EvidenceKind::AgentAsserted`] lets the model assert edges, and a blanket
/// delete would let a 0.60 guess erase a 0.98 compiler-resolved fact. The rule
/// is therefore about *confidence*, not about who is writing —
///
/// * lower-confidence incumbent → deleted, the new edge is written;
/// * equal-or-higher incumbent → kept, the new edge is **not** written
///   (`skipped_outranked`), so re-asserting is idempotent rather than duplicating.
///
/// Endpoints are resolved by `symbol_key`; an edge whose endpoints are not both
/// present is skipped. Each applied edge enqueues a `SymbolChanged` event for its
/// `from` node, in the same transaction as the writes.
pub async fn upsert_semantic_edges(
    pool: &SqlitePool,
    repository: RepositoryId,
    revision: &GitRevision,
    edges: &[SemanticEdge],
) -> Result<SemanticUpsertOutcome, CodeGraphError> {
    let results = apply_semantic_edges(pool, repository, revision, edges).await?;
    let mut outcome = SemanticUpsertOutcome::default();
    for result in results {
        match result {
            AssertionResult::Applied | AssertionResult::Superseded { .. } => outcome.applied += 1,
            AssertionResult::Outranked { .. } => outcome.skipped_outranked += 1,
            AssertionResult::Unresolved { .. } | AssertionResult::Ambiguous { .. } => {
                outcome.skipped_unresolved += 1;
            }
        }
    }
    Ok(outcome)
}

/// The shared write core: one [`AssertionResult`] per input edge, in input order.
/// All of it in one transaction, as before.
async fn apply_semantic_edges(
    pool: &SqlitePool,
    repository: RepositoryId,
    revision: &GitRevision,
    edges: &[SemanticEdge],
) -> Result<Vec<AssertionResult>, CodeGraphError> {
    let now = Utc::now();
    let created_at = now.to_rfc3339();
    let mut results = Vec::with_capacity(edges.len());

    let mut tx = pool.begin().await?;
    for edge in edges {
        let from = resolve_node_id(&mut *tx, repository, &edge.from_symbol_key).await?;
        let to = resolve_node_id(&mut *tx, repository, &edge.to_symbol_key).await?;
        let (Some(from), Some(to)) = (from, to) else {
            let missing = if from.is_none() {
                &edge.from_symbol_key
            } else {
                &edge.to_symbol_key
            };
            results.push(AssertionResult::Unresolved {
                symbol: missing.clone(),
                candidates: Vec::new(),
            });
            continue;
        };

        let record = CodeEdge {
            from,
            to,
            relation: edge.relation,
            confidence: edge.confidence,
            evidence_kind: edge.evidence_kind,
            evidence: edge.evidence.clone(),
            revision: revision.clone(),
        };
        results.push(match fold_edge(&mut tx, &record, &created_at).await? {
            EdgeFold::Written { superseded } => {
                outbox::enqueue(&mut *tx, &KnowledgeIndexEvent::SymbolChanged(from), now).await?;
                match superseded {
                    Some((previous, previous_confidence)) => AssertionResult::Superseded {
                        previous,
                        previous_confidence,
                    },
                    None => AssertionResult::Applied,
                }
            }
            EdgeFold::Outranked {
                existing,
                existing_confidence,
            } => AssertionResult::Outranked {
                existing,
                existing_confidence,
            },
        });
    }
    tx.commit().await?;
    Ok(results)
}

/// What folding one edge did to the incumbent holding its `(from, to, relation)`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum EdgeFold {
    /// The edge was written. `superseded` names the strictly less-confident
    /// incumbent it replaced, when there was one.
    Written {
        superseded: Option<(EvidenceKind, f32)>,
    },
    /// The edge was **not** written: an incumbent of equal or greater confidence
    /// already holds the triple.
    Outranked {
        existing: EvidenceKind,
        existing_confidence: f32,
    },
}

/// Fold ONE edge into `code_edges` under the confidence rule, inside `tx`.
///
/// **The single spelling of the ordering.** Every writer goes through here — the
/// agent-assertion path, the LSP/compiler path, and the tree-sitter reparse in
/// [`upsert_file_graph`] — because a `(from, to, relation)` triple holds at most
/// one edge and which one it holds must not depend on who wrote last:
///
/// * incumbent strictly less confident → deleted, this edge written
///   ([`EdgeFold::Written`] naming it);
/// * incumbent equal or more confident → kept, this edge **not** written, so
///   re-asserting and re-parsing are both idempotent rather than duplicating.
///
/// The reparse path used to bare-`INSERT` here instead. Its own delete is scoped
/// to `syntax_inferred` (a save must not erase an agent's or an LSP's work), so
/// an incumbent from any other layer survived it and the insert landed *beside*
/// it: two rows for one triple, listed twice by `graph show` and returned twice
/// by `graph.callers_of`.
///
/// The caller owns the transaction and any outbox event, because the two paths
/// enqueue differently (per applied edge vs. per durable node).
async fn fold_edge(
    tx: &mut sqlx::SqliteConnection,
    edge: &CodeEdge,
    created_at: &str,
) -> Result<EdgeFold, CodeGraphError> {
    // Read the incumbent before touching it: its kind and confidence are what
    // a caller needs to explain either outcome.
    let incumbent: Option<(String, f64)> = sqlx::query_as(
        "SELECT evidence_kind, confidence FROM code_edges \
         WHERE from_node = ? AND to_node = ? AND relation = ? \
         ORDER BY confidence DESC LIMIT 1",
    )
    .bind(edge.from.to_string())
    .bind(edge.to.to_string())
    .bind(scalar(&edge.relation))
    .fetch_optional(&mut *tx)
    .await?;
    let incumbent = incumbent
        .map(|(kind, confidence)| {
            from_scalar::<EvidenceKind>(&kind).map(|kind| (kind, confidence as f32))
        })
        .transpose()?;

    // An incumbent at least as confident keeps the triple. Inserting beside
    // it would shadow a stronger fact with a weaker duplicate; deleting it —
    // which the semantic path did unconditionally — would let a 0.40 agent
    // guess erase a 0.98 compiler-resolved fact.
    if let Some((existing, existing_confidence)) = incumbent {
        if existing_confidence >= edge.confidence {
            return Ok(EdgeFold::Outranked {
                existing,
                existing_confidence,
            });
        }
    }

    let removed = sqlx::query(
        "DELETE FROM code_edges \
         WHERE from_node = ? AND to_node = ? AND relation = ? AND confidence < ?",
    )
    .bind(edge.from.to_string())
    .bind(edge.to.to_string())
    .bind(scalar(&edge.relation))
    .bind(f64::from(edge.confidence))
    .execute(&mut *tx)
    .await?;

    let evidence_json = match &edge.evidence {
        Some(evidence) => Some(serde_json::to_string(evidence)?),
        None => None,
    };
    sqlx::query(
        "INSERT INTO code_edges \
         (id, from_node, to_node, relation, confidence, evidence_kind, evidence_artifact, \
          revision, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(edge.from.to_string())
    .bind(edge.to.to_string())
    .bind(scalar(&edge.relation))
    .bind(f64::from(edge.confidence))
    .bind(scalar(&edge.evidence_kind))
    .bind(evidence_json)
    .bind(&edge.revision.0)
    .bind(created_at)
    .execute(&mut *tx)
    .await?;

    Ok(EdgeFold::Written {
        superseded: incumbent.filter(|_| removed.rows_affected() > 0),
    })
}

// --------------------------------------------------------------------------
// Revision-aware queries (STEP 4.5) — power staleness and the Phase 5 planner
// --------------------------------------------------------------------------

/// A symbol's identity + signature at one point in time. Keyed for change
/// detection by `qualified_name` (the granularity a `{{ symbol:… }}` document
/// reference names), with `signature_hash` as the value a change is detected on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolSnapshot {
    pub qualified_name: String,
    pub kind: CodeNodeKind,
    pub source_path: String,
    pub signature_hash: Option<String>,
}

/// What changed between two symbol snapshots (`graph.changed_between`). A signature
/// change is a `modified`; a disappearance is a `removed` — both flag stale docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SymbolDelta {
    pub added: Vec<SymbolSnapshot>,
    pub removed: Vec<SymbolSnapshot>,
    /// `(before, after)` for symbols present in both whose signature changed.
    pub modified: Vec<(SymbolSnapshot, SymbolSnapshot)>,
}

/// The current symbol snapshot for a repository (every graph node's identity +
/// signature). Callers capture this at a commit (e.g. on publish) and diff a
/// later snapshot against it with [`changed_between`].
pub async fn symbol_snapshot(
    pool: &SqlitePool,
    repository: RepositoryId,
) -> Result<Vec<SymbolSnapshot>, CodeGraphError> {
    Ok(nodes(pool, repository)
        .await?
        .into_iter()
        .map(|node| SymbolSnapshot {
            qualified_name: node.key.qualified_name,
            kind: node.key.kind,
            source_path: node.key.source_path,
            signature_hash: node.key.signature_hash.map(|h| h.0),
        })
        .collect())
}

/// Diff two symbol snapshots (the `graph.changed_between(rev_a, rev_b)` query,
/// with each revision represented by its snapshot). Symbols are matched by
/// `qualified_name`; a differing `signature_hash` is a `modified`.
#[must_use]
pub fn changed_between(before: &[SymbolSnapshot], after: &[SymbolSnapshot]) -> SymbolDelta {
    // Symbol identity is file-scoped: two symbols may share a `qualified_name` in
    // different files, so key the diff by `(source_path, qualified_name)`. Keying
    // by name alone would collapse them — hiding a removal or attributing a
    // `modified` to the wrong file.
    let index = |snaps: &[SymbolSnapshot]| -> HashMap<(String, String), SymbolSnapshot> {
        snaps
            .iter()
            .map(|s| ((s.source_path.clone(), s.qualified_name.clone()), s.clone()))
            .collect()
    };
    let before_by = index(before);
    let after_by = index(after);

    let mut delta = SymbolDelta::default();
    for (key, after_sym) in &after_by {
        match before_by.get(key) {
            None => delta.added.push(after_sym.clone()),
            Some(before_sym) if before_sym.signature_hash != after_sym.signature_hash => {
                delta.modified.push((before_sym.clone(), after_sym.clone()));
            }
            Some(_) => {}
        }
    }
    for (key, before_sym) in &before_by {
        if !after_by.contains_key(key) {
            delta.removed.push(before_sym.clone());
        }
    }
    // Stable order so callers/tests see deterministic results.
    delta
        .added
        .sort_by(|a, b| a.qualified_name.cmp(&b.qualified_name));
    delta
        .removed
        .sort_by(|a, b| a.qualified_name.cmp(&b.qualified_name));
    delta
        .modified
        .sort_by(|a, b| a.1.qualified_name.cmp(&b.1.qualified_name));
    delta
}

/// The direct callers of the symbol identified by `symbol_key` — nodes with a
/// `Calls`/`References` edge into it (`graph.callers_of`).
pub async fn callers_of(
    pool: &SqlitePool,
    repository: RepositoryId,
    symbol_key: &str,
) -> Result<Vec<CodeNode>, CodeGraphError> {
    let rows: Vec<NodeRow> = sqlx::query_as(
        "SELECT n.id AS id, n.language AS language, n.package AS package, \
                n.source_path AS source_path, n.qualified_name AS qualified_name, \
                n.kind AS kind, n.signature_hash AS signature_hash, n.revision AS revision, \
                n.created_at AS created_at \
         FROM code_nodes n \
         JOIN code_edges e ON e.from_node = n.id \
         JOIN code_nodes t ON e.to_node = t.id \
         WHERE t.repository = ? AND t.symbol_key = ? \
           AND e.relation IN ('calls', 'references') \
         ORDER BY n.created_at ASC, n.id ASC",
    )
    .bind(repository.to_string())
    .bind(symbol_key)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(|r| r.into_node(repository)).collect()
}

/// The transitive blast radius of a symbol: every node that reaches it through up
/// to `depth` layers of `Calls`/`References` edges (`graph.blast_radius`). The
/// target itself is excluded.
pub async fn blast_radius(
    pool: &SqlitePool,
    repository: RepositoryId,
    symbol_key: &str,
    depth: usize,
) -> Result<Vec<CodeNode>, CodeGraphError> {
    let Some(start) = resolve_node_id(pool, repository, symbol_key).await? else {
        return Ok(Vec::new());
    };
    let reached = reverse_reachable(pool, repository, &[start], depth).await?;
    nodes_by_ids(pool, repository, &reached).await
}

/// The tests covering a path: `Test` nodes that reach any symbol defined in
/// `path` through up to `depth` layers of `Calls`/`References` edges, PLUS the
/// tests defined in `path` itself (`graph.tests_covering`).
///
/// The same-file half matters more than the traversal in Rust: a `#[cfg(test)]
/// mod tests` lives in the file it exercises, so its tests are seeds of the walk
/// and [`reverse_reachable`] deliberately excludes seeds — asking "what tests
/// cover `engine.rs`" would answer "none" for the overwhelmingly common layout.
pub async fn tests_covering(
    pool: &SqlitePool,
    repository: RepositoryId,
    path: &str,
    depth: usize,
) -> Result<Vec<CodeNode>, CodeGraphError> {
    let seeds: Vec<CodeNodeId> = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM code_nodes WHERE repository = ? AND source_path = ?",
    )
    .bind(repository.to_string())
    .bind(path)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(id,)| CodeNodeId::from_str(&id))
    .collect::<Result<_, _>>()?;
    covering_tests(pool, repository, &seeds, depth).await
}

/// The `Test` nodes among `seeds` and everything that reverse-reaches them.
async fn covering_tests(
    pool: &SqlitePool,
    repository: RepositoryId,
    seeds: &[CodeNodeId],
    depth: usize,
) -> Result<Vec<CodeNode>, CodeGraphError> {
    if seeds.is_empty() {
        return Ok(Vec::new());
    }
    let mut reached = reverse_reachable(pool, repository, seeds, depth).await?;
    reached.extend_from_slice(seeds);
    Ok(nodes_by_ids(pool, repository, &reached)
        .await?
        .into_iter()
        .filter(|n| n.key.kind == CodeNodeKind::Test)
        .collect())
}

/// How many ids ride in one statement.
///
/// SQLite refuses a statement with more host parameters than
/// `SQLITE_MAX_VARIABLE_NUMBER` (32 766 since 3.32; 999 before it). The id sets
/// here have no ceiling — one machine-generated bindings module folds to five
/// figures of symbols in ONE file, and the retirement sweep bound every id
/// TWICE, so a little over 16 000 nodes in a file failed the statement and
/// errored the whole reparse rather than that one file. Chunking is what makes
/// these statements expressible, not a tuning knob.
const SQL_ID_CHUNK: usize = 400;

fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Retire every node still recorded for `path` that this parse did not re-see,
/// with its incident edges in both directions.
///
/// The retiring set is computed here rather than expressed as `id NOT IN
/// (every id this parse kept)`: the kept set is the unbounded one (it is the
/// file's whole symbol table), the retiring set is normally EMPTY, and a
/// complement cannot be chunked — every chunk of a `NOT IN` would delete rows
/// the other chunks keep. Reading the file's current ids first turns one
/// unbounded statement into zero statements in the common case.
async fn retire_absent_nodes(
    conn: &mut sqlx::SqliteConnection,
    repository: RepositoryId,
    path: &str,
    kept: &[CodeNodeId],
) -> Result<(), CodeGraphError> {
    let recorded: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM code_nodes WHERE repository = ? AND source_path = ?")
            .bind(repository.to_string())
            .bind(path)
            .fetch_all(&mut *conn)
            .await?;
    let kept: HashSet<CodeNodeId> = kept.iter().copied().collect();
    let mut retiring = Vec::new();
    for (id,) in recorded {
        let id = CodeNodeId::from_str(&id)?;
        if !kept.contains(&id) {
            retiring.push(id);
        }
    }
    if retiring.is_empty() {
        return Ok(());
    }
    for chunk in retiring.chunks(SQL_ID_CHUNK) {
        let placeholders = placeholders(chunk.len());
        // Edges first, in both directions: `code_edges` has no ON DELETE
        // CASCADE and foreign keys are ON, so one surviving reference makes the
        // node delete fail and takes the whole reparse with it.
        let edges_sql = format!(
            "DELETE FROM code_edges WHERE from_node IN ({placeholders}) \
             OR to_node IN ({placeholders})"
        );
        let mut edges_query = sqlx::query(&edges_sql);
        for _ in 0..2 {
            for id in chunk {
                edges_query = edges_query.bind(id.to_string());
            }
        }
        edges_query.execute(&mut *conn).await?;

        let nodes_sql = format!("DELETE FROM code_nodes WHERE id IN ({placeholders})");
        let mut nodes_query = sqlx::query(&nodes_sql);
        for id in chunk {
            nodes_query = nodes_query.bind(id.to_string());
        }
        nodes_query.execute(&mut *conn).await?;
    }
    Ok(())
}

/// Resolve a node id from its stable `symbol_key`, within an executor.
async fn resolve_node_id(
    executor: impl sqlx::SqliteExecutor<'_>,
    repository: RepositoryId,
    symbol_key: &str,
) -> Result<Option<CodeNodeId>, CodeGraphError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT id FROM code_nodes WHERE repository = ? AND symbol_key = ?")
            .bind(repository.to_string())
            .bind(symbol_key)
            .fetch_optional(executor)
            .await?;
    row.map(|(id,)| CodeNodeId::from_str(&id))
        .transpose()
        .map_err(CodeGraphError::from)
}

/// BFS over reverse (`caller → callee`) edges from `seeds`, up to `depth` layers.
/// Returns every node reached, excluding the seeds themselves.
async fn reverse_reachable(
    pool: &SqlitePool,
    repository: RepositoryId,
    seeds: &[CodeNodeId],
    depth: usize,
) -> Result<Vec<CodeNodeId>, CodeGraphError> {
    let mut visited: std::collections::HashSet<CodeNodeId> = seeds.iter().copied().collect();
    let mut frontier: Vec<CodeNodeId> = seeds.to_vec();
    for _ in 0..depth {
        let mut next = Vec::new();
        for caller in direct_caller_ids(pool, repository, &frontier).await? {
            if visited.insert(caller) {
                next.push(caller);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok(visited
        .into_iter()
        .filter(|id| !seeds.contains(id))
        .collect())
}

/// The ids with a `Calls`/`References` edge directly into `node`, **scoped to
/// `repository`**.
///
/// The scope is applied during the walk, not after it: a semantic (LSP) edge can
/// cross checkouts served by one daemon, and filtering only at the final
/// `nodes_by_ids` — as this did before — let the BFS spend its depth budget on
/// nodes in another repository and then drop them silently, so a `blast_radius`
/// could come back short with no indication why (2026-08-13 review, F10).
/// One round trip per FRONTIER, not per frontier node. A BFS layer over a hub
/// symbol is hundreds of nodes wide and this used to be a separate
/// `SELECT ... WHERE to_node = ?` for each of them, so a depth-5 blast radius
/// spent its time in round trips rather than in the index.
async fn direct_caller_ids(
    pool: &SqlitePool,
    repository: RepositoryId,
    nodes: &[CodeNodeId],
) -> Result<Vec<CodeNodeId>, CodeGraphError> {
    let mut callers = Vec::new();
    for chunk in nodes.chunks(SQL_ID_CHUNK) {
        let placeholders = placeholders(chunk.len());
        let sql = format!(
            "SELECT DISTINCT e.from_node FROM code_edges e \
             JOIN code_nodes n ON e.from_node = n.id \
             WHERE e.to_node IN ({placeholders}) AND n.repository = ? \
             AND e.relation IN ('calls', 'references')"
        );
        let mut query = sqlx::query_as::<_, (String,)>(&sql);
        for node in chunk {
            query = query.bind(node.to_string());
        }
        let rows = query.bind(repository.to_string()).fetch_all(pool).await?;
        for (id,) in rows {
            callers.push(CodeNodeId::from_str(&id)?);
        }
    }
    Ok(callers)
}

/// Fetch full [`CodeNode`]s for a set of ids (order by creation for determinism).
async fn nodes_by_ids(
    pool: &SqlitePool,
    repository: RepositoryId,
    ids: &[CodeNodeId],
) -> Result<Vec<CodeNode>, CodeGraphError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // Chunked for the same reason as everything else that binds a set of ids:
    // a blast radius can reach more ids than SQLite will accept parameters for,
    // and the id set is the one input to this module with no ceiling on it.
    let mut rows = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(SQL_ID_CHUNK) {
        let placeholders = placeholders(chunk.len());
        let sql = format!(
            "SELECT id, language, package, source_path, qualified_name, kind, signature_hash, \
             revision, created_at FROM code_nodes WHERE repository = ? AND id IN ({placeholders})"
        );
        let mut query = sqlx::query_as::<_, NodeRow>(&sql).bind(repository.to_string());
        for id in chunk {
            query = query.bind(id.to_string());
        }
        rows.extend(query.fetch_all(pool).await?);
    }
    // Ordered here rather than in SQL: the ORDER BY was per-statement, so it
    // could not order across chunks. Same key, same determinism.
    rows.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    rows.into_iter().map(|r| r.into_node(repository)).collect()
}

// --------------------------------------------------------------------------
// The named query surface (`graph.callers_of` / `blast_radius` /
// `tests_covering`) — what a tool, the TUI, or a CLI subcommand asks
// --------------------------------------------------------------------------

/// How many nodes one [`GraphAnswer`] discloses. A blast radius over a hub
/// symbol can reach hundreds of nodes; an answer that spends the whole context
/// window is worse than a truncated one that says it was truncated, so the
/// answer carries `total` alongside the disclosed slice.
pub const GRAPH_ANSWER_LIMIT: usize = 40;

/// Ceiling on traversal depth. Each layer is a BFS round-trip per frontier node,
/// so an unbounded depth on a large graph is a database stampede; five layers is
/// far past the useful answer for "what breaks if I change this".
pub const GRAPH_MAX_DEPTH: usize = 5;

/// How many alternative symbols an ambiguous or missed lookup names back, so a
/// caller that guessed the wrong name gets a next step instead of "not found".
pub const GRAPH_CANDIDATE_LIMIT: usize = 10;

/// A question the code graph can answer about a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphQuestion {
    /// A bounded, paginated query reached through the existing graph seam.
    Traverse(GraphTraversal),
    /// Who calls this symbol directly (`graph.callers_of`).
    CallersOf { symbol: String },
    /// Everything that transitively reaches this symbol (`graph.blast_radius`).
    BlastRadius { symbol: String, depth: usize },
    /// Which tests reach any symbol defined in this file (`graph.tests_covering`).
    TestsCovering { path: String, depth: usize },
}

/// One disclosed symbol in a [`GraphAnswer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphHit {
    /// Stable identity accepted by the next traversal query.
    pub id: CodeNodeId,
    /// Validated by the daemon against the current source, when available.
    pub location: Option<codypendent_protocol::CodeGraphSourceLocation>,
    pub qualified_name: String,
    pub kind: CodeNodeKind,
    pub source_path: String,
    /// The revision this node was last folded at. A `…+workdir` suffix means it
    /// came from an uncommitted working-tree edit picked up by the watcher.
    pub revision: String,
}

/// The answer to one [`GraphQuestion`]: what was asked, what it resolved to, and
/// the (bounded) symbols found — plus the candidates to try when nothing matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphAnswer {
    /// Absent for legacy query variants; never an invented pagination state.
    pub traversal: Option<GraphTraversalMetadata>,
    /// A one-line rendering of the question, so the answer stands alone.
    pub question: String,
    /// The symbols/files the question actually resolved to. Empty means the
    /// lookup found nothing — see `candidates`.
    pub targets: Vec<String>,
    /// Near-miss symbol names, offered when `targets` is empty or the match was
    /// ambiguous.
    pub candidates: Vec<String>,
    /// The disclosed slice of the result, at most [`GRAPH_ANSWER_LIMIT`].
    pub hits: Vec<GraphHit>,
    /// How many results existed before truncation.
    pub total: usize,
}

impl GraphAnswer {
    /// True when `hits` is a truncated view of `total`.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.total > self.hits.len()
    }

    /// Render the answer as the compact text block a tool result or a CLI/TUI
    /// pane shows. Deliberately plain: every line is `kind qualified_name —
    /// path`, so a model can quote a symbol straight back into a follow-up query
    /// and a human can paste a path into an editor.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.question);
        out.push('\n');
        if !self.targets.is_empty() {
            out.push_str(&format!("resolved to: {}\n", self.targets.join(", ")));
        }
        if self.hits.is_empty() {
            out.push_str(if self.traversal.is_some() && self.total > 0 {
                "no results at this offset\n"
            } else {
                "no results\n"
            });
            if !self.candidates.is_empty() {
                out.push_str(&format!("did you mean: {}\n", self.candidates.join(", ")));
            }
            self.render_traversal(&mut out);
            return out;
        }
        out.push_str(&format!(
            "{} result{}{}\n",
            self.total,
            if self.total == 1 { "" } else { "s" },
            if let Some(page) = &self.traversal {
                format!(
                    " (showing {} at offset {}{})",
                    self.hits.len(),
                    page.offset,
                    if page.total_is_exact {
                        ""
                    } else {
                        "; count is a lower bound"
                    }
                )
            } else if self.truncated() {
                format!(" (showing the first {})", self.hits.len())
            } else {
                String::new()
            }
        ));
        for hit in &self.hits {
            out.push_str(&format!(
                "  {} {} — {} @{}\n",
                kind_label(hit.kind),
                hit.qualified_name,
                hit.source_path,
                hit.revision
            ));
            if self.traversal.is_some() {
                out.push_str(&format!("    node_id: {}\n", hit.id));
                if let Some(location) = &hit.location {
                    out.push_str(&format!(
                        "    definition: {}:{}:{}\n",
                        location.source_path, location.line, location.column
                    ));
                }
            }
        }
        self.render_traversal(&mut out);
        out
    }

    fn render_traversal(&self, out: &mut String) {
        if let Some(page) = &self.traversal {
            out.push_str(&format!(
                "page: offset {}, limit {}",
                page.offset, page.limit
            ));
            if let Some(next) = page.next_offset {
                out.push_str(&format!(", next_offset {next}"));
            }
            out.push('\n');
            if !page.resolved_ids.is_empty() {
                out.push_str(&format!(
                    "resolved node IDs: {}\n",
                    page.resolved_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !page.total_is_exact {
                out.push_str("Traversal budget reached; result count is a lower bound. Narrow the symbol/path or follow one returned node.\n");
            }
        }
    }
}

/// The lower-case scalar name of a node kind, as the query surface prints it.
fn kind_label(kind: CodeNodeKind) -> String {
    let raw = scalar(&kind);
    if raw.is_empty() {
        format!("{kind:?}").to_lowercase()
    } else {
        raw
    }
}

/// The seam a tool/CLI calls to ask the code graph a question. Lives here rather
/// than in the runtime so the query layer, its bounds, and its rendering are one
/// unit: the caller supplies a repository ROOT (what a run knows) and the
/// implementation resolves the repository identity and the pool it owns.
#[async_trait::async_trait]
pub trait CodeGraphQueries: Send + Sync {
    /// Answer `question` about the checkout at `repository_root`. Errors are
    /// human strings — the caller renders them into a tool failure.
    async fn ask(
        &self,
        repository_root: &Path,
        question: GraphQuestion,
    ) -> Result<GraphAnswer, String>;
}

/// Every node whose `qualified_name` matches `name`, best match first.
///
/// Three tiers, tried in order and never mixed: an exact `qualified_name`, then
/// a last-segment match (`Engine::tick` for `tick`), then a substring. A model
/// or a user names a symbol the way it appears in the source, not by the
/// `source_path|package::name#Kind@hash` composite the graph keys on, so this is
/// the translation layer every query goes through.
pub async fn find_symbols(
    pool: &SqlitePool,
    repository: RepositoryId,
    name: &str,
    limit: usize,
) -> Result<Vec<CodeNode>, CodeGraphError> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(Vec::new());
    }
    // `_` and `%` are LIKE wildcards and `_` is pervasive in Rust identifiers, so
    // escape both (plus the escape character itself) before interpolating.
    let escaped = name
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    // The last-segment tier tries BOTH separators the graph stores: `Engine::tick`
    // for Rust and `Engine.tick` for Python/TypeScript. With only `::` a Python
    // repository answered every `graph.*` question from the substring tier or not
    // at all.
    let tiers: [(&str, Vec<String>); 3] = [
        ("qualified_name = ?", vec![name.to_string()]),
        (
            "(qualified_name LIKE ? ESCAPE '\\' OR qualified_name LIKE ? ESCAPE '\\')",
            vec![format!("%::{escaped}"), format!("%.{escaped}")],
        ),
        (
            "qualified_name LIKE ? ESCAPE '\\'",
            vec![format!("%{escaped}%")],
        ),
    ];
    for (predicate, patterns) in tiers {
        let sql = format!(
            "SELECT id, language, package, source_path, qualified_name, kind, signature_hash, \
                    revision, created_at \
             FROM code_nodes WHERE repository = ? AND {predicate} \
             ORDER BY created_at ASC, id ASC LIMIT ?"
        );
        let mut query = sqlx::query_as::<_, NodeRow>(&sql).bind(repository.to_string());
        for pattern in &patterns {
            query = query.bind(pattern);
        }
        let rows: Vec<NodeRow> = query.bind(limit as i64).fetch_all(pool).await?;
        if !rows.is_empty() {
            return rows
                .into_iter()
                .map(|row| row.into_node(repository))
                .collect();
        }
    }
    Ok(Vec::new())
}

/// Answer a [`GraphQuestion`] against `repository`'s graph.
///
/// Depth is clamped to [`GRAPH_MAX_DEPTH`] and results to [`GRAPH_ANSWER_LIMIT`]
/// rather than rejected, so a caller that asks for `depth: 99` gets a bounded
/// answer instead of an error it has to learn to avoid.
pub async fn answer(
    pool: &SqlitePool,
    repository: RepositoryId,
    question: &GraphQuestion,
) -> Result<GraphAnswer, CodeGraphError> {
    match question {
        GraphQuestion::Traverse(request) => traversal::answer(pool, repository, request).await,
        GraphQuestion::CallersOf { symbol } => {
            let (targets, seeds, candidates) = resolve_seeds(pool, repository, symbol).await?;
            let mut reached = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for caller in direct_caller_ids(pool, repository, &seeds).await? {
                if !seeds.contains(&caller) && seen.insert(caller) {
                    reached.push(caller);
                }
            }
            let nodes = nodes_by_ids(pool, repository, &reached).await?;
            Ok(assemble(
                format!("callers of `{symbol}`"),
                targets,
                candidates,
                nodes,
            ))
        }
        GraphQuestion::BlastRadius { symbol, depth } => {
            let depth = (*depth).clamp(1, GRAPH_MAX_DEPTH);
            let (targets, seeds, candidates) = resolve_seeds(pool, repository, symbol).await?;
            let reached = reverse_reachable(pool, repository, &seeds, depth).await?;
            let nodes = nodes_by_ids(pool, repository, &reached).await?;
            Ok(assemble(
                format!("blast radius of `{symbol}` (depth {depth})"),
                targets,
                candidates,
                nodes,
            ))
        }
        GraphQuestion::TestsCovering { path, depth } => {
            let depth = (*depth).clamp(1, GRAPH_MAX_DEPTH);
            let (targets, seeds) = resolve_path_seeds(pool, repository, path).await?;
            let candidates = if seeds.is_empty() {
                nearby_paths(pool, repository, path).await?
            } else {
                Vec::new()
            };
            let nodes = covering_tests(pool, repository, &seeds, depth).await?;
            Ok(assemble(
                format!("tests covering `{path}` (depth {depth})"),
                targets,
                candidates,
                nodes,
            ))
        }
    }
}

/// Resolve a symbol name to its matching node ids, plus the names they resolved
/// to and (when nothing matched) the near-miss candidates to suggest.
async fn resolve_seeds(
    pool: &SqlitePool,
    repository: RepositoryId,
    symbol: &str,
) -> Result<(Vec<String>, Vec<CodeNodeId>, Vec<String>), CodeGraphError> {
    let matched = find_symbols(pool, repository, symbol, GRAPH_CANDIDATE_LIMIT).await?;
    if matched.is_empty() {
        return Ok((
            Vec::new(),
            Vec::new(),
            nearby_symbols(pool, repository, symbol).await?,
        ));
    }
    let targets = matched
        .iter()
        .map(|n| format!("{} ({})", n.key.qualified_name, n.key.source_path))
        .collect();
    let seeds = matched.iter().map(|n| n.id).collect();
    Ok((targets, seeds, Vec::new()))
}

/// Resolve a file path to the ids of every symbol defined in it. Accepts a
/// repo-relative path, or a suffix of one (`policy.rs` finds
/// `crates/routing/src/policy.rs`) — a user or model rarely types the full path.
async fn resolve_path_seeds(
    pool: &SqlitePool,
    repository: RepositoryId,
    path: &str,
) -> Result<(Vec<String>, Vec<CodeNodeId>), CodeGraphError> {
    let path = path.trim().trim_start_matches("./");
    if path.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let escaped = path
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    for (predicate, pattern) in [
        ("source_path = ?".to_string(), path.to_string()),
        (
            "source_path LIKE ? ESCAPE '\\'".to_string(),
            format!("%{escaped}"),
        ),
    ] {
        let sql =
            format!("SELECT id, source_path FROM code_nodes WHERE repository = ? AND {predicate}");
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(&sql)
            .bind(repository.to_string())
            .bind(&pattern)
            .fetch_all(pool)
            .await?;
        if !rows.is_empty() {
            let mut paths: Vec<String> = rows
                .iter()
                .filter_map(|(_, p)| p.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            paths.truncate(GRAPH_CANDIDATE_LIMIT);
            let ids = rows
                .iter()
                .map(|(id, _)| CodeNodeId::from_str(id))
                .collect::<Result<Vec<_>, _>>()?;
            return Ok((paths, ids));
        }
    }
    Ok((Vec::new(), Vec::new()))
}

/// Symbol names sharing a prefix with a failed lookup — the "did you mean".
async fn nearby_symbols(
    pool: &SqlitePool,
    repository: RepositoryId,
    symbol: &str,
) -> Result<Vec<String>, CodeGraphError> {
    let head: String = symbol.trim().chars().take(3).collect();
    if head.is_empty() {
        return Ok(Vec::new());
    }
    let escaped = head
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT qualified_name FROM code_nodes \
         WHERE repository = ? AND qualified_name LIKE ? ESCAPE '\\' \
         ORDER BY qualified_name ASC LIMIT ?",
    )
    .bind(repository.to_string())
    .bind(format!("%{escaped}%"))
    .bind(GRAPH_CANDIDATE_LIMIT as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}

/// Indexed paths sharing a component with a failed path lookup.
async fn nearby_paths(
    pool: &SqlitePool,
    repository: RepositoryId,
    path: &str,
) -> Result<Vec<String>, CodeGraphError> {
    let stem = Path::new(path.trim())
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if stem.is_empty() {
        return Ok(Vec::new());
    }
    let escaped = stem
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT source_path FROM code_nodes \
         WHERE repository = ? AND source_path LIKE ? ESCAPE '\\' \
         ORDER BY source_path ASC LIMIT ?",
    )
    .bind(repository.to_string())
    .bind(format!("%{escaped}%"))
    .bind(GRAPH_CANDIDATE_LIMIT as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(path,)| path).collect())
}

/// Project resolved nodes into a bounded [`GraphAnswer`].
fn assemble(
    question: String,
    targets: Vec<String>,
    candidates: Vec<String>,
    nodes: Vec<CodeNode>,
) -> GraphAnswer {
    let total = nodes.len();
    let hits = nodes
        .into_iter()
        .take(GRAPH_ANSWER_LIMIT)
        .map(|node| GraphHit {
            id: node.id,
            location: None,
            qualified_name: node.key.qualified_name,
            kind: node.key.kind,
            source_path: node.key.source_path,
            revision: node.revision.0,
        })
        .collect();
    GraphAnswer {
        traversal: None,
        question,
        targets,
        candidates,
        hits,
        total,
    }
}

// --------------------------------------------------------------------------
// Incremental pipeline — filesystem watcher (minimal)
// --------------------------------------------------------------------------

/// A filesystem watcher whose watched subtrees the caller chooses one at a time.
///
/// Wrapping `notify`'s watcher keeps the dependency inside this crate (the
/// daemon drives it without naming `notify`), and — more importantly — lets the
/// caller arm *selected* subtrees rather than one recursive watch over the
/// repository root. That distinction is not cosmetic: `inotify` registers one
/// kernel watch per directory, and a recursive watch on a Rust checkout root
/// registers thousands for `target/` alone (5065 in this repository) that can
/// only ever produce events the caller discards. Filtering events is not enough;
/// the watches must not be taken in the first place.
///
/// The watcher owns its own background thread and stops when dropped.
pub struct GraphWatcher {
    inner: notify::RecommendedWatcher,
}

impl GraphWatcher {
    /// Start watching `path`, recursively or not. Idempotent per path in
    /// `notify`'s own semantics (re-watching replaces the previous mode).
    pub fn watch_subtree(&mut self, path: &Path, recursive: bool) -> Result<(), CodeGraphError> {
        let mode = if recursive {
            notify::RecursiveMode::Recursive
        } else {
            notify::RecursiveMode::NonRecursive
        };
        notify::Watcher::watch(&mut self.inner, path, mode)?;
        Ok(())
    }
}

/// True for an event kind that can change what the PARSER sees in a file.
///
/// `notify`'s inotify mask includes `IN_OPEN`, so merely **reading** a watched
/// file produces an event. A caller that reparses on every event therefore
/// re-triggers itself the instant it opens the file it was just told about — an
/// endless reparse loop running at the debounce frequency forever, which is
/// exactly what the daemon did before this filter (observed 2026-08-13: one
/// uncommitted edit produced a batch every 420 ms indefinitely). Metadata-only
/// changes (`chmod`, an atime bump under a non-`relatime` mount) are dropped for
/// the same reason: they cannot change a symbol.
fn is_content_change(kind: &notify::EventKind) -> bool {
    use notify::event::ModifyKind;
    use notify::EventKind;
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Modify(_) => true,
        EventKind::Access(_) => false,
        // `Any`/`Other` come from backends that do not classify (the polling
        // fallback). Treat them as changes: a spurious reparse is cheap, a
        // silently dropped edit is the bug this whole pipeline exists to fix.
        EventKind::Any | EventKind::Other => true,
    }
}

/// Build a watcher that forwards the paths of **content-changing** filesystem
/// events to `handler`, watching nothing until [`GraphWatcher::watch_subtree`]
/// is called.
///
/// The handler receives plain paths, not `notify` types: kind classification
/// (see [`is_content_change`]) and error suppression belong here, next to the
/// dependency, and keeping them here is what lets the daemon drive a watcher
/// without naming `notify` at all.
///
/// Intentionally minimal beyond that: it does not itself reparse — a caller
/// debounces the paths, applies the ignore/generated-file policy, and calls
/// [`upsert_file_graph`] per changed file to produce a [`GraphDelta`]. The
/// daemon's `scan::arm_watcher` is that caller.
pub fn watcher<F>(mut handler: F) -> Result<GraphWatcher, CodeGraphError>
where
    F: FnMut(Vec<std::path::PathBuf>) + Send + 'static,
{
    let inner = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if !is_content_change(&event.kind) {
            return;
        }
        handler(event.paths);
    })?;
    Ok(GraphWatcher { inner })
}

/// Arm a recursive filesystem watcher over `root` — [`watcher`] plus a single
/// recursive subtree. Convenience for a caller that genuinely wants the whole
/// tree; the daemon deliberately does not (see [`GraphWatcher`]).
pub fn watch<F>(root: &Path, handler: F) -> Result<GraphWatcher, CodeGraphError>
where
    F: FnMut(Vec<std::path::PathBuf>) + Send + 'static,
{
    let mut watcher = watcher(handler)?;
    watcher.watch_subtree(root, true)?;
    Ok(watcher)
}

// --------------------------------------------------------------------------
// Row (de)serialization
// --------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct NodeRow {
    id: String,
    language: String,
    package: Option<String>,
    source_path: Option<String>,
    qualified_name: String,
    kind: String,
    signature_hash: Option<String>,
    revision: String,
    /// Carried so a multi-statement read can restore the single-statement
    /// `ORDER BY created_at, id` after the fact — the order decides which
    /// nodes a truncated answer discloses, so it is not free to drift.
    created_at: String,
}

impl NodeRow {
    fn into_node(self, repository: RepositoryId) -> Result<CodeNode, CodeGraphError> {
        Ok(CodeNode {
            id: CodeNodeId::from_str(&self.id)?,
            key: SymbolKey {
                repository,
                language: LanguageId(self.language),
                package: self.package,
                // Legacy rows written before the column existed read as "" — the
                // startup scan rebuilds them with a real path.
                source_path: self.source_path.unwrap_or_default(),
                qualified_name: self.qualified_name,
                kind: from_scalar(&self.kind)?,
                signature_hash: self.signature_hash.map(ContentHash),
            },
            revision: GitRevision(self.revision),
        })
    }
}

#[derive(sqlx::FromRow)]
struct EdgeRow {
    from_node: String,
    to_node: String,
    relation: String,
    confidence: f64,
    evidence_kind: String,
    evidence_artifact: Option<String>,
    revision: String,
}

impl EdgeRow {
    fn into_edge(self) -> Result<CodeEdge, CodeGraphError> {
        let evidence = match self.evidence_artifact {
            Some(json) => Some(serde_json::from_str::<EvidenceRef>(&json)?),
            None => None,
        };
        Ok(CodeEdge {
            from: CodeNodeId::from_str(&self.from_node)?,
            to: CodeNodeId::from_str(&self.to_node)?,
            relation: from_scalar(&self.relation)?,
            confidence: self.confidence as f32,
            evidence_kind: from_scalar(&self.evidence_kind)?,
            evidence,
            revision: GitRevision(self.revision),
        })
    }
}

/// Encode a `#[serde(rename_all = "snake_case")]` unit enum as its scalar column
/// string. These enums always serialize to a JSON string, so the fallback is
/// unreachable; it keeps the helper total rather than panicking.
fn scalar<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => text,
        _ => String::new(),
    }
}

/// Decode a scalar column string back into its enum, matching [`scalar`].
fn from_scalar<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, CodeGraphError> {
    Ok(serde_json::from_value(serde_json::Value::String(
        text.to_owned(),
    ))?)
}

// --------------------------------------------------------------------------
// Parsing — the pure tree-sitter walk
// --------------------------------------------------------------------------

/// One node produced by the walk, before persistence assigns it a [`CodeNodeId`].
struct BuiltNode {
    key: SymbolKey,
    /// Durable symbols defined *in this file* are `owned` (File/Module/Type/
    /// Trait/Function/Method/Constant/Test); synthesized import and unresolved-
    /// call targets are references (`ExternalDependency`, not owned).
    owned: bool,
}

/// One edge produced by the walk, endpoints as indices into `BuiltGraph::nodes`.
struct BuiltEdge {
    from: usize,
    to: usize,
    relation: CodeRelation,
    confidence: f32,
    evidence_kind: EvidenceKind,
    /// The salient byte span for this edge's evidence (the call site for a
    /// `Calls`, the `use` for an `Imports`, the child item otherwise). Encoded
    /// into the evidence `source_path` as `path#start-end`.
    site_start: usize,
    site_end: usize,
}

/// The parsed graph for a single file.
struct BuiltGraph {
    /// A lightweight descriptive ref to the file itself, shared by every edge's
    /// evidence. There is no artifact store in this crate — the ref is purely
    /// descriptive (its id is derived from the content hash so re-parsing the
    /// same bytes yields an identical ref).
    file_artifact: ArtifactRef,
    nodes: Vec<BuiltNode>,
    edges: Vec<BuiltEdge>,
}

/// A pending call, recorded during the walk and resolved once every owned node
/// is known (so a call to a function defined later in the file still resolves).
struct PendingCall {
    from: usize,
    /// The callee's simple (last-segment) name, used for within-file resolution.
    simple: String,
    /// The callee's full written path, used to name an unresolved reference node.
    written: String,
    is_method: bool,
    site_start: usize,
    site_end: usize,
}

/// Recursion ceiling for AST descent (item nesting and expression trees). The
/// visitors recurse per nesting level, so without a guard one pathologically
/// deep source file (tens of thousands of nested modules or expressions) would
/// overflow the stack and abort the whole daemon — an uncatchable crash from a
/// single crafted file in a scanned repository. Real code nests a handful of
/// levels; past the ceiling the visitor stops descending (graceful truncation
/// of that file's graph, never a crash).
const MAX_PARSE_DEPTH: usize = 512;

/// The lexical context threaded down the walk.
#[derive(Clone)]
struct Ctx {
    /// AST descent depth, bounded by [`MAX_PARSE_DEPTH`].
    depth: usize,
    /// The `::`-scope for qualified names (module/type/trait segments).
    scope_path: Vec<String>,
    /// Nearest enclosing File/Module/Trait node — the `Contains` parent.
    container: usize,
    /// The `Defines` parent (File/Module for free items, Trait for trait items,
    /// the enclosing module/file for impl items since `impl` is not a node kind).
    definer: usize,
    /// The enclosing function/method/test node, if any — the `from` of `Calls`.
    current_fn: Option<usize>,
    /// Whether we are inside an `impl`/`trait` body (associated fns are Methods).
    associated: bool,
    /// Whether we are inside a `#[cfg(test)]` module (fns become Tests).
    in_test: bool,
}

struct Builder<'a> {
    repository: RepositoryId,
    /// Which grammar produced the tree, and therefore which node kinds the
    /// visitors match and which separator qualified names are built with.
    language: Language,
    /// The repo-relative path being parsed; stamped onto every node's key so a
    /// file's symbols are identified independently of any other file's.
    path: &'a str,
    source: &'a str,
    /// Whether `path` names a test file, by each ecosystem's own convention.
    /// Rust marks tests with an attribute; Python and JavaScript mark them by
    /// file name, so the path is the only signal available at the syntax layer.
    test_file: bool,
    nodes: Vec<BuiltNode>,
    edges: Vec<BuiltEdge>,
    pending_calls: Vec<PendingCall>,
    /// Dedup by `SymbolKey::stable_key()` so repeated imports/calls reuse a node.
    index: HashMap<String, usize>,
}

/// Parse `source` into a [`BuiltGraph`]. Deterministic: identical bytes always
/// produce identical nodes, edges, and evidence.
///
/// The grammar is chosen by [`language_for`]; a path no grammar handles is an
/// explicit [`CodeGraphError::UnsupportedLanguage`], never a silently empty
/// graph. Parsing a Python file with the Rust grammar (which is what a single
/// hardcoded grammar amounts to once the scanner offers more than `.rs`) yields
/// an error tree that looks exactly like an empty file.
fn build_file_graph(
    repository: RepositoryId,
    path: &str,
    source: &str,
) -> Result<BuiltGraph, CodeGraphError> {
    let language =
        language_for(Path::new(path)).ok_or_else(|| CodeGraphError::UnsupportedLanguage {
            path: path.to_owned(),
        })?;
    let digest = Sha256::digest(source.as_bytes());
    let file_artifact = ArtifactRef {
        id: ArtifactId(Uuid::from_slice(&digest[..16])?),
        media_type: language.media_type().to_owned(),
        byte_length: source.len() as u64,
        sha256: hex::encode(digest),
        sensitivity: DataClassification::Internal,
    };

    let mut parser = Parser::new();
    parser
        .set_language(&language.grammar())
        .map_err(|e| CodeGraphError::Parse(e.to_string()))?;
    let tree = parser
        .parse(source.as_bytes(), None)
        .ok_or_else(|| CodeGraphError::Parse("tree-sitter returned no tree".to_owned()))?;

    let mut builder = Builder {
        repository,
        language,
        path,
        source,
        test_file: is_test_path(language, path),
        nodes: Vec::new(),
        edges: Vec::new(),
        pending_calls: Vec::new(),
        index: HashMap::new(),
    };

    // The File node anchors the graph; its qualified name is the path, and every
    // node's key carries this path as its `source_path` — so a symbol's identity
    // is scoped to its file (a same-named symbol in another file is distinct) and
    // a rename to a new path yields fresh nodes for the file.
    let file_idx = builder.add_node(
        builder.make_key(path.to_owned(), CodeNodeKind::File, None),
        true,
    );
    let root_ctx = Ctx {
        depth: 0,
        scope_path: Vec::new(),
        container: file_idx,
        definer: file_idx,
        current_fn: None,
        associated: false,
        in_test: false,
    };
    builder.visit_items(tree.root_node(), &root_ctx);
    builder.resolve_calls();

    Ok(BuiltGraph {
        file_artifact,
        nodes: builder.nodes,
        edges: builder.edges,
    })
}

impl Builder<'_> {
    fn make_key(
        &self,
        qualified_name: String,
        kind: CodeNodeKind,
        signature_hash: Option<ContentHash>,
    ) -> SymbolKey {
        SymbolKey {
            repository: self.repository,
            language: self.language.id(),
            package: None,
            source_path: self.path.to_owned(),
            qualified_name,
            kind,
            signature_hash,
        }
    }

    /// `scope::name` / `scope.name`, in this file's language.
    fn qualify(&self, scope: &[String], name: &str) -> String {
        join_qualified(&scope.join(self.language.separator()), self.language, name)
    }

    /// Dispatch an item list to the visitor for this file's grammar. Every
    /// language walks the same `Ctx` and produces the same node kinds and the
    /// same four relations; only the tree-sitter node names differ.
    fn visit_items(&mut self, list: Node, ctx: &Ctx) {
        if ctx.depth >= MAX_PARSE_DEPTH {
            return; // graceful truncation, never a stack overflow (see the const)
        }
        match self.language {
            Language::Rust => self.visit_rust_items(list, ctx),
            Language::Python => self.visit_python_items(list, ctx),
            Language::TypeScript | Language::Tsx | Language::JavaScript => {
                self.visit_ecma_items(list, ctx);
            }
        }
    }

    fn add_node(&mut self, key: SymbolKey, owned: bool) -> usize {
        let stable = key.stable_key();
        if let Some(&idx) = self.index.get(&stable) {
            if owned && !self.nodes[idx].owned {
                self.nodes[idx].owned = true;
            }
            return idx;
        }
        let idx = self.nodes.len();
        self.nodes.push(BuiltNode { key, owned });
        self.index.insert(stable, idx);
        idx
    }

    fn add_edge(&mut self, from: usize, to: usize, relation: CodeRelation, span: (usize, usize)) {
        self.edges.push(BuiltEdge {
            from,
            to,
            relation,
            confidence: 1.0,
            evidence_kind: EvidenceKind::SyntaxInferred,
            site_start: span.0,
            site_end: span.1,
        });
    }

    fn text(&self, node: Node) -> String {
        node.utf8_text(self.source.as_bytes())
            .unwrap_or("")
            .to_owned()
    }

    /// Iterate the named children of a Rust item list (`source_file` /
    /// `declaration_list`), attaching each pending attribute run to the item that
    /// follows it, and dispatch each item to its handler.
    fn visit_rust_items(&mut self, list: Node, ctx: &Ctx) {
        let children: Vec<Node> = list.named_children(&mut list.walk()).collect();
        let mut pending: Vec<String> = Vec::new();
        for child in children {
            match child.kind() {
                "attribute_item" => pending.push(self.text(child)),
                "line_comment" | "block_comment" => {} // keep the pending attrs
                "function_item" | "function_signature_item" => {
                    self.handle_fn(child, ctx, &pending);
                    pending.clear();
                }
                "mod_item" => {
                    self.handle_mod(child, ctx, &pending);
                    pending.clear();
                }
                "struct_item" | "enum_item" | "union_item" | "type_item" => {
                    self.handle_type(child, ctx);
                    pending.clear();
                }
                "trait_item" => {
                    self.handle_trait(child, ctx);
                    pending.clear();
                }
                "impl_item" => {
                    self.handle_impl(child, ctx);
                    pending.clear();
                }
                "const_item" | "static_item" => {
                    self.handle_const(child, ctx);
                    pending.clear();
                }
                "use_declaration" => {
                    self.handle_use(child, ctx);
                    pending.clear();
                }
                _ => pending.clear(),
            }
        }
    }

    fn handle_fn(&mut self, node: Node, ctx: &Ctx, attrs: &[String]) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let qualified = self.qualify(&ctx.scope_path, &name);
        let is_test = ctx.in_test || attrs.iter().any(|a| is_test_attr(a));
        let kind = if is_test {
            CodeNodeKind::Test
        } else if ctx.associated {
            CodeNodeKind::Method
        } else {
            CodeNodeKind::Function
        };
        let signature = self.signature_hash(node);
        let idx = self.add_node(self.make_key(qualified, kind, Some(signature)), true);
        let span = (node.start_byte(), node.end_byte());
        self.add_edge(ctx.container, idx, CodeRelation::Contains, span);
        self.add_edge(ctx.definer, idx, CodeRelation::Defines, span);

        if let Some(body) = node.child_by_field_name("body") {
            let mut body_ctx = ctx.clone();
            body_ctx.current_fn = Some(idx);
            self.collect_calls(body, &body_ctx, 0);
        }
    }

    fn handle_mod(&mut self, node: Node, ctx: &Ctx, attrs: &[String]) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let qualified = self.qualify(&ctx.scope_path, &name);
        let idx = self.add_node(self.make_key(qualified, CodeNodeKind::Module, None), true);
        let span = (node.start_byte(), node.end_byte());
        self.add_edge(ctx.container, idx, CodeRelation::Contains, span);
        self.add_edge(ctx.definer, idx, CodeRelation::Defines, span);

        if let Some(body) = node.child_by_field_name("body") {
            let is_cfg_test = attrs.iter().any(|a| is_cfg_test_attr(a));
            let mut child_scope = ctx.scope_path.clone();
            child_scope.push(name);
            let mod_ctx = Ctx {
                depth: ctx.depth + 1,
                scope_path: child_scope,
                container: idx,
                definer: idx,
                current_fn: None,
                associated: false,
                in_test: ctx.in_test || is_cfg_test,
            };
            self.visit_items(body, &mod_ctx);
        }
    }

    fn handle_type(&mut self, node: Node, ctx: &Ctx) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let qualified = self.qualify(&ctx.scope_path, &name);
        let idx = self.add_node(self.make_key(qualified, CodeNodeKind::Type, None), true);
        let span = (node.start_byte(), node.end_byte());
        self.add_edge(ctx.container, idx, CodeRelation::Contains, span);
        self.add_edge(ctx.definer, idx, CodeRelation::Defines, span);
    }

    fn handle_trait(&mut self, node: Node, ctx: &Ctx) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let qualified = self.qualify(&ctx.scope_path, &name);
        let idx = self.add_node(
            self.make_key(qualified, CodeNodeKind::TraitOrInterface, None),
            true,
        );
        let span = (node.start_byte(), node.end_byte());
        self.add_edge(ctx.container, idx, CodeRelation::Contains, span);
        self.add_edge(ctx.definer, idx, CodeRelation::Defines, span);

        if let Some(body) = node.child_by_field_name("body") {
            let mut child_scope = ctx.scope_path.clone();
            child_scope.push(name);
            let trait_ctx = Ctx {
                depth: ctx.depth + 1,
                scope_path: child_scope,
                container: idx,
                definer: idx,
                current_fn: None,
                associated: true,
                in_test: ctx.in_test,
            };
            self.visit_items(body, &trait_ctx);
        }
    }

    fn handle_impl(&mut self, node: Node, ctx: &Ctx) {
        // `impl` is not a durable node kind, so it contributes no node. Its
        // associated items are scoped under the self type's name and are
        // Contained/Defined by the impl's own enclosing module/file.
        let Some(type_name) = node
            .child_by_field_name("type")
            .map(|n| impl_type_name(&self.text(n)))
        else {
            return;
        };
        if let Some(body) = node.child_by_field_name("body") {
            let mut child_scope = ctx.scope_path.clone();
            child_scope.push(type_name);
            let impl_ctx = Ctx {
                depth: ctx.depth + 1,
                scope_path: child_scope,
                container: ctx.container,
                definer: ctx.definer,
                current_fn: None,
                associated: true,
                in_test: ctx.in_test,
            };
            self.visit_items(body, &impl_ctx);
        }
    }

    fn handle_const(&mut self, node: Node, ctx: &Ctx) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let qualified = self.qualify(&ctx.scope_path, &name);
        let idx = self.add_node(self.make_key(qualified, CodeNodeKind::Constant, None), true);
        let span = (node.start_byte(), node.end_byte());
        self.add_edge(ctx.container, idx, CodeRelation::Contains, span);
        self.add_edge(ctx.definer, idx, CodeRelation::Defines, span);
    }

    fn handle_use(&mut self, node: Node, ctx: &Ctx) {
        let Some(argument) = node.child_by_field_name("argument") else {
            return;
        };
        let mut paths = Vec::new();
        self.expand_use(argument, "", &mut paths);
        let span = (node.start_byte(), node.end_byte());
        for path in paths {
            self.add_import(path, ctx.container, span);
        }
    }

    /// Flatten a `use` tree into the full written paths it brings in (one per
    /// leaf), e.g. `use a::b::{C, D as E};` → `["a::b::C", "a::b::D"]`.
    fn expand_use(&self, node: Node, prefix: &str, out: &mut Vec<String>) {
        match node.kind() {
            "scoped_use_list" => {
                let path = node
                    .child_by_field_name("path")
                    .map(|n| self.text(n))
                    .unwrap_or_default();
                let next = join_qualified(prefix, self.language, &path);
                if let Some(list) = node.child_by_field_name("list") {
                    for child in list.named_children(&mut list.walk()) {
                        self.expand_use(child, &next, out);
                    }
                }
            }
            "use_list" => {
                for child in node.named_children(&mut node.walk()) {
                    self.expand_use(child, prefix, out);
                }
            }
            "use_as_clause" => {
                let path = node
                    .child_by_field_name("path")
                    .map(|n| self.text(n))
                    .unwrap_or_default();
                out.push(join_qualified(prefix, self.language, &path));
            }
            _ => out.push(join_qualified(prefix, self.language, &self.text(node))),
        }
    }

    /// Descend a function body recording every call against the enclosing
    /// function. Nested item declarations are skipped (their calls belong to
    /// them, not the outer fn); closures and arrow bodies are descended into.
    fn collect_calls(&mut self, node: Node, ctx: &Ctx, depth: usize) {
        if depth >= MAX_PARSE_DEPTH {
            return; // graceful truncation, never a stack overflow (see the const)
        }
        let (call_kind, nested_items) = match self.language {
            Language::Rust => ("call_expression", &["function_item"][..]),
            Language::Python => (
                "call",
                &[
                    "function_definition",
                    "class_definition",
                    "decorated_definition",
                ][..],
            ),
            Language::TypeScript | Language::Tsx | Language::JavaScript => (
                "call_expression",
                &[
                    "function_declaration",
                    "generator_function_declaration",
                    "class_declaration",
                    "abstract_class_declaration",
                    "method_definition",
                ][..],
            ),
        };
        let children: Vec<Node> = node.named_children(&mut node.walk()).collect();
        for child in children {
            if nested_items.contains(&child.kind()) {
                continue;
            }
            if child.kind() == call_kind {
                if let Some(function) = child.child_by_field_name("function") {
                    if let (Some(from), Some((simple, written, is_method))) =
                        (ctx.current_fn, self.callee_name(function))
                    {
                        self.pending_calls.push(PendingCall {
                            from,
                            simple,
                            written,
                            is_method,
                            site_start: child.start_byte(),
                            site_end: child.end_byte(),
                        });
                    }
                }
            }
            self.collect_calls(child, ctx, depth + 1);
        }
    }

    /// The `(simple_name, written_path, is_method)` of a call's callee, or `None`
    /// when the callee is not a plain name/path/member (e.g. a call on a call).
    fn callee_name(&self, function: Node) -> Option<(String, String, bool)> {
        match self.language {
            Language::Rust => self.rust_callee_name(function),
            Language::Python => self.python_callee_name(function),
            Language::TypeScript | Language::Tsx | Language::JavaScript => {
                self.ecma_callee_name(function)
            }
        }
    }

    fn rust_callee_name(&self, function: Node) -> Option<(String, String, bool)> {
        match function.kind() {
            "identifier" => {
                let name = self.text(function);
                Some((name.clone(), name, false))
            }
            "scoped_identifier" => {
                let written = self.text(function);
                let simple = function
                    .child_by_field_name("name")
                    .map(|n| self.text(n))
                    .unwrap_or_else(|| last_segment(&written).to_owned());
                Some((simple, written, false))
            }
            "field_expression" => {
                let field = function.child_by_field_name("field")?;
                if field.kind() != "field_identifier" {
                    return None; // tuple index `.0`, not a method call
                }
                let name = self.text(field);
                Some((name.clone(), name, true))
            }
            "generic_function" => {
                let inner = function.child_by_field_name("function")?;
                self.rust_callee_name(inner)
            }
            _ => None,
        }
    }

    /// Python: `f()` is an identifier, `obj.f()` / `mod.f()` an `attribute`. The
    /// receiver's type is unknown at the syntax layer, so `attribute` is reported
    /// as a method call and resolves only against an unambiguous local method.
    fn python_callee_name(&self, function: Node) -> Option<(String, String, bool)> {
        match function.kind() {
            "identifier" => {
                let name = self.text(function);
                Some((name.clone(), name, false))
            }
            "attribute" => {
                let attribute = function.child_by_field_name("attribute")?;
                Some((self.text(attribute), self.text(function), true))
            }
            _ => None,
        }
    }

    /// TypeScript/JavaScript: `f()` is an identifier, `obj.f()` a
    /// `member_expression`. `obj?.f()` is the same node with an optional chain.
    fn ecma_callee_name(&self, function: Node) -> Option<(String, String, bool)> {
        match function.kind() {
            "identifier" => {
                let name = self.text(function);
                Some((name.clone(), name, false))
            }
            "member_expression" => {
                let property = function.child_by_field_name("property")?;
                if property.kind() != "property_identifier" {
                    return None; // a computed member `obj[expr]()`, not a name
                }
                Some((self.text(property), self.text(function), true))
            }
            "parenthesized_expression" => {
                let inner = function.named_child(0)?;
                self.ecma_callee_name(inner)
            }
            _ => None,
        }
    }

    // ----------------------------------------------------------------------
    // Python
    // ----------------------------------------------------------------------

    /// Iterate a Python `module` or `block`, dispatching each statement. Class
    /// bodies come through here too, with `associated` set, so a `def` inside a
    /// class becomes a [`CodeNodeKind::Method`] exactly as a Rust `impl` fn does.
    fn visit_python_items(&mut self, list: Node, ctx: &Ctx) {
        let children: Vec<Node> = list.named_children(&mut list.walk()).collect();
        for child in children {
            match child.kind() {
                "function_definition" => self.handle_python_fn(child, ctx),
                "class_definition" => self.handle_python_class(child, ctx),
                // `@decorator def f(): …` — the decorators are not nodes; the
                // definition they wrap is.
                "decorated_definition" => {
                    if let Some(inner) = child.child_by_field_name("definition") {
                        match inner.kind() {
                            "function_definition" => self.handle_python_fn(inner, ctx),
                            "class_definition" => self.handle_python_class(inner, ctx),
                            _ => {}
                        }
                    }
                }
                "import_statement" | "future_import_statement" => {
                    self.handle_python_import(child, ctx);
                }
                "import_from_statement" => self.handle_python_from_import(child, ctx),
                // A module-level `NAME = …` is the Python analogue of a Rust
                // `const`. Inside a class it is a field, which the Rust walk does
                // not record either, so it is skipped for the same reason.
                "expression_statement" if !ctx.associated => {
                    self.handle_python_assignment(child, ctx);
                }
                _ => {}
            }
        }
    }

    fn handle_python_fn(&mut self, node: Node, ctx: &Ctx) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let qualified = self.qualify(&ctx.scope_path, &name);
        let kind = if self.test_file && name.starts_with("test") {
            CodeNodeKind::Test
        } else if ctx.associated {
            CodeNodeKind::Method
        } else {
            CodeNodeKind::Function
        };
        let signature = self.signature_hash(node);
        let idx = self.add_node(self.make_key(qualified, kind, Some(signature)), true);
        let span = (node.start_byte(), node.end_byte());
        self.add_edge(ctx.container, idx, CodeRelation::Contains, span);
        self.add_edge(ctx.definer, idx, CodeRelation::Defines, span);

        if let Some(body) = node.child_by_field_name("body") {
            let mut body_ctx = ctx.clone();
            body_ctx.current_fn = Some(idx);
            self.collect_calls(body, &body_ctx, 0);
        }
    }

    fn handle_python_class(&mut self, node: Node, ctx: &Ctx) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let qualified = self.qualify(&ctx.scope_path, &name);
        let idx = self.add_node(self.make_key(qualified, CodeNodeKind::Type, None), true);
        let span = (node.start_byte(), node.end_byte());
        self.add_edge(ctx.container, idx, CodeRelation::Contains, span);
        self.add_edge(ctx.definer, idx, CodeRelation::Defines, span);

        if let Some(body) = node.child_by_field_name("body") {
            let mut child_scope = ctx.scope_path.clone();
            child_scope.push(name);
            let class_ctx = Ctx {
                depth: ctx.depth + 1,
                scope_path: child_scope,
                container: idx,
                definer: idx,
                current_fn: None,
                associated: true,
                in_test: ctx.in_test,
            };
            self.visit_items(body, &class_ctx);
        }
    }

    /// `import a.b`, `import a.b as c` — one reference per imported module.
    fn handle_python_import(&mut self, node: Node, ctx: &Ctx) {
        let span = (node.start_byte(), node.end_byte());
        let children: Vec<Node> = node.named_children(&mut node.walk()).collect();
        for child in children {
            let written = match child.kind() {
                "dotted_name" => self.text(child),
                "aliased_import" => match child.child_by_field_name("name") {
                    Some(name) => self.text(name),
                    None => continue,
                },
                _ => continue,
            };
            self.add_import(written, ctx.container, span);
        }
    }

    /// `from a.b import C, D` → `a.b.C`, `a.b.D`; `from . import x` → `.x`;
    /// `from a import *` → `a`. Mirrors the Rust `use`-tree flattening: one
    /// reference node per imported leaf, named by its full written path.
    fn handle_python_from_import(&mut self, node: Node, ctx: &Ctx) {
        let span = (node.start_byte(), node.end_byte());
        let module = node
            .child_by_field_name("module_name")
            .map(|n| self.text(n))
            .unwrap_or_default();
        let leaves: Vec<String> = node
            .children_by_field_name("name", &mut node.walk())
            .map(|leaf| match leaf.kind() {
                "aliased_import" => leaf
                    .child_by_field_name("name")
                    .map_or_else(|| self.text(leaf), |n| self.text(n)),
                _ => self.text(leaf),
            })
            .collect();
        if leaves.is_empty() {
            // `from a import *`: the module itself is the whole import.
            if !module.is_empty() {
                self.add_import(module, ctx.container, span);
            }
            return;
        }
        for leaf in leaves {
            let written = join_qualified(&module, self.language, &leaf);
            self.add_import(written, ctx.container, span);
        }
    }

    /// A module-level `NAME = …`, recorded as a [`CodeNodeKind::Constant`] the
    /// way a Rust `const`/`static` is. Only a plain `identifier` target: a tuple
    /// unpack or a subscript assignment names no single durable symbol.
    fn handle_python_assignment(&mut self, node: Node, ctx: &Ctx) {
        let Some(assignment) = node.named_child(0).filter(|n| n.kind() == "assignment") else {
            return;
        };
        let Some(left) = assignment
            .child_by_field_name("left")
            .filter(|n| n.kind() == "identifier")
        else {
            return;
        };
        let name = self.text(left);
        let qualified = self.qualify(&ctx.scope_path, &name);
        let idx = self.add_node(self.make_key(qualified, CodeNodeKind::Constant, None), true);
        let span = (node.start_byte(), node.end_byte());
        self.add_edge(ctx.container, idx, CodeRelation::Contains, span);
        self.add_edge(ctx.definer, idx, CodeRelation::Defines, span);
    }

    // ----------------------------------------------------------------------
    // TypeScript / TSX / JavaScript
    // ----------------------------------------------------------------------

    /// Iterate an ECMAScript `program`, `statement_block`, `class_body`,
    /// `interface_body` or namespace body. One visitor for all three grammars:
    /// the TypeScript-only kinds (`interface_declaration`, `enum_declaration`, …)
    /// simply never appear in a JavaScript tree.
    fn visit_ecma_items(&mut self, list: Node, ctx: &Ctx) {
        let children: Vec<Node> = list.named_children(&mut list.walk()).collect();
        for child in children {
            self.visit_ecma_item(child, ctx);
        }
    }

    fn visit_ecma_item(&mut self, node: Node, ctx: &Ctx) {
        match node.kind() {
            // `export …` / `declare …` wrap the declaration that matters. A bare
            // `export { a }` or `export … from "x"` carries no declaration.
            "export_statement" | "ambient_declaration" => {
                if let Some(declaration) = node.child_by_field_name("declaration") {
                    let mut inner = ctx.clone();
                    inner.depth += 1;
                    self.visit_ecma_item(declaration, &inner);
                } else if node.child_by_field_name("source").is_some() {
                    self.handle_ecma_import(node, ctx);
                }
            }
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                self.handle_ecma_fn(node, ctx, None);
            }
            "class_declaration" | "abstract_class_declaration" => {
                self.handle_ecma_class(node, ctx, CodeNodeKind::Type);
            }
            "interface_declaration" => {
                self.handle_ecma_class(node, ctx, CodeNodeKind::TraitOrInterface);
            }
            "enum_declaration" => self.handle_ecma_class(node, ctx, CodeNodeKind::Type),
            "type_alias_declaration" => self.handle_ecma_type_alias(node, ctx),
            // `namespace X { … }` / `module X { … }`.
            "internal_module" | "module" => self.handle_ecma_namespace(node, ctx),
            "lexical_declaration" | "variable_declaration" => {
                self.handle_ecma_variables(node, ctx);
            }
            // Class members.
            "method_definition" => self.handle_ecma_fn(node, ctx, None),
            "public_field_definition" | "field_definition" => {
                self.handle_ecma_field(node, ctx);
            }
            // Interface members.
            "method_signature" | "abstract_method_signature" => {
                self.handle_ecma_fn(node, ctx, None);
            }
            "import_statement" => self.handle_ecma_import(node, ctx),
            _ => {}
        }
    }

    /// A function/method declaration. `body_start` overrides where the signature
    /// ends, for an arrow function whose body belongs to the arrow rather than to
    /// the declarator the signature is read from.
    fn handle_ecma_fn(&mut self, node: Node, ctx: &Ctx, body: Option<Node>) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let qualified = self.qualify(&ctx.scope_path, &name);
        let kind = if self.test_file {
            // Jest/Vitest cases are anonymous callbacks to `it`/`test`, so no
            // declaration is ever the test. Everything declared in a test file is
            // test scaffolding, which is what `tests_covering` should surface.
            CodeNodeKind::Test
        } else if ctx.associated {
            CodeNodeKind::Method
        } else {
            CodeNodeKind::Function
        };
        let body = body.or_else(|| node.child_by_field_name("body"));
        let signature = self.signature_hash_between(
            node.start_byte(),
            body.map_or_else(|| node.end_byte(), |b| b.start_byte()),
        );
        let idx = self.add_node(self.make_key(qualified, kind, Some(signature)), true);
        let span = (node.start_byte(), node.end_byte());
        self.add_edge(ctx.container, idx, CodeRelation::Contains, span);
        self.add_edge(ctx.definer, idx, CodeRelation::Defines, span);

        if let Some(body) = body {
            let mut body_ctx = ctx.clone();
            body_ctx.current_fn = Some(idx);
            self.collect_calls(body, &body_ctx, 0);
        }
    }

    /// A class, interface or enum, plus its members.
    fn handle_ecma_class(&mut self, node: Node, ctx: &Ctx, kind: CodeNodeKind) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let qualified = self.qualify(&ctx.scope_path, &name);
        let idx = self.add_node(self.make_key(qualified, kind, None), true);
        let span = (node.start_byte(), node.end_byte());
        self.add_edge(ctx.container, idx, CodeRelation::Contains, span);
        self.add_edge(ctx.definer, idx, CodeRelation::Defines, span);

        if let Some(body) = node.child_by_field_name("body") {
            let mut child_scope = ctx.scope_path.clone();
            child_scope.push(name);
            let member_ctx = Ctx {
                depth: ctx.depth + 1,
                scope_path: child_scope,
                container: idx,
                definer: idx,
                current_fn: None,
                associated: true,
                in_test: ctx.in_test,
            };
            self.visit_items(body, &member_ctx);
        }
    }

    fn handle_ecma_type_alias(&mut self, node: Node, ctx: &Ctx) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let qualified = self.qualify(&ctx.scope_path, &name);
        let idx = self.add_node(self.make_key(qualified, CodeNodeKind::Type, None), true);
        let span = (node.start_byte(), node.end_byte());
        self.add_edge(ctx.container, idx, CodeRelation::Contains, span);
        self.add_edge(ctx.definer, idx, CodeRelation::Defines, span);
    }

    fn handle_ecma_namespace(&mut self, node: Node, ctx: &Ctx) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let qualified = self.qualify(&ctx.scope_path, &name);
        let idx = self.add_node(
            self.make_key(qualified, CodeNodeKind::Namespace, None),
            true,
        );
        let span = (node.start_byte(), node.end_byte());
        self.add_edge(ctx.container, idx, CodeRelation::Contains, span);
        self.add_edge(ctx.definer, idx, CodeRelation::Defines, span);

        if let Some(body) = node.child_by_field_name("body") {
            let mut child_scope = ctx.scope_path.clone();
            child_scope.push(name);
            let namespace_ctx = Ctx {
                depth: ctx.depth + 1,
                scope_path: child_scope,
                container: idx,
                definer: idx,
                current_fn: None,
                associated: false,
                in_test: ctx.in_test,
            };
            self.visit_items(body, &namespace_ctx);
        }
    }

    /// `const f = () => …` / `const f = function () {…}` is a function
    /// declaration in every way that matters — and the dominant style in React
    /// code, so treating it as an opaque binding would lose most of a TSX file.
    /// Any other declarator is a module-level `Constant` (`const`) or `Global`
    /// (`let`/`var`), the ECMAScript analogue of Rust's `const`/`static`.
    fn handle_ecma_variables(&mut self, node: Node, ctx: &Ctx) {
        let is_const = self
            .source
            .get(node.start_byte()..node.end_byte())
            .is_some_and(|text| text.trim_start().starts_with("const"));
        let declarators: Vec<Node> = node
            .named_children(&mut node.walk())
            .filter(|child| child.kind() == "variable_declarator")
            .collect();
        for declarator in declarators {
            let Some(name) = declarator.child_by_field_name("name") else {
                continue;
            };
            if name.kind() != "identifier" {
                continue; // a destructuring pattern names no single symbol
            }
            let value = declarator.child_by_field_name("value");
            let function_body = value.filter(|v| {
                matches!(
                    v.kind(),
                    "arrow_function" | "function_expression" | "function" | "generator_function"
                )
            });
            match function_body {
                Some(function) => {
                    self.handle_ecma_fn(declarator, ctx, function.child_by_field_name("body"));
                }
                None => {
                    let qualified = self.qualify(&ctx.scope_path, &self.text(name));
                    let kind = if is_const {
                        CodeNodeKind::Constant
                    } else {
                        CodeNodeKind::Global
                    };
                    let idx = self.add_node(self.make_key(qualified, kind, None), true);
                    let span = (declarator.start_byte(), declarator.end_byte());
                    self.add_edge(ctx.container, idx, CodeRelation::Contains, span);
                    self.add_edge(ctx.definer, idx, CodeRelation::Defines, span);
                }
            }
        }
    }

    /// A class field holding an arrow function (`handle = () => …`) is a method;
    /// a plain data field is not recorded, matching the Rust walk's treatment of
    /// struct fields.
    fn handle_ecma_field(&mut self, node: Node, ctx: &Ctx) {
        let Some(value) = node.child_by_field_name("value") else {
            return;
        };
        if !matches!(
            value.kind(),
            "arrow_function" | "function_expression" | "function" | "generator_function"
        ) {
            return;
        }
        self.handle_ecma_fn(node, ctx, value.child_by_field_name("body"));
    }

    /// `import { a, b as c } from "./m"` → `./m.a`, `./m.c`;
    /// `import D from "./m"` → `./m.D`; `import "./m"` → `./m`. One reference
    /// node per imported binding, named by module + binding, mirroring the Rust
    /// `use`-tree flattening.
    fn handle_ecma_import(&mut self, node: Node, ctx: &Ctx) {
        let Some(source) = node.child_by_field_name("source") else {
            return;
        };
        let module = self.text(source);
        let module = module.trim_matches(['"', '\'', '`']).to_owned();
        if module.is_empty() {
            return;
        }
        let span = (node.start_byte(), node.end_byte());
        let mut bindings = Vec::new();
        self.collect_ecma_bindings(node, &mut bindings);
        if bindings.is_empty() {
            self.add_import(module, ctx.container, span);
            return;
        }
        for binding in bindings {
            let written = join_qualified(&module, self.language, &binding);
            self.add_import(written, ctx.container, span);
        }
    }

    /// The local names an import/re-export clause introduces. Descends the clause
    /// only — the module `source` is handled by the caller.
    fn collect_ecma_bindings(&self, node: Node, out: &mut Vec<String>) {
        let children: Vec<Node> = node.named_children(&mut node.walk()).collect();
        for child in children {
            match child.kind() {
                "import_clause" | "named_imports" | "export_clause" => {
                    self.collect_ecma_bindings(child, out);
                }
                "import_specifier" | "export_specifier" => {
                    let named = child
                        .child_by_field_name("alias")
                        .or_else(|| child.child_by_field_name("name"));
                    if let Some(named) = named {
                        out.push(self.text(named));
                    }
                }
                // `import D from "m"` (default) and `import * as N from "m"`.
                "identifier" => out.push(self.text(child)),
                "namespace_import" => {
                    if let Some(name) = child.named_child(0) {
                        out.push(self.text(name));
                    }
                }
                _ => {}
            }
        }
    }

    /// Add an `Imports` edge to a synthesized reference node for `written`.
    fn add_import(&mut self, written: String, container: usize, span: (usize, usize)) {
        let idx = self.add_node(
            self.make_key(written, CodeNodeKind::ExternalDependency, None),
            false,
        );
        self.add_edge(container, idx, CodeRelation::Imports, span);
    }

    /// Resolve every pending call to a target node and emit its `Calls` edge.
    /// A plain call whose simple name matches an owned function/method/test
    /// (preferring one in the caller's module) resolves to it; a method call
    /// resolves only when exactly one owned method has that name; everything else
    /// points at a synthesized `ExternalDependency` node named by the written
    /// path. Resolution is within-file only, which keeps a single-file reparse
    /// independent of the rest of the graph.
    fn resolve_calls(&mut self) {
        let callables: Vec<Callable> = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| {
                n.owned
                    && matches!(
                        n.key.kind,
                        CodeNodeKind::Function | CodeNodeKind::Method | CodeNodeKind::Test
                    )
            })
            .map(|(index, n)| Callable {
                index,
                simple: last_segment(&n.key.qualified_name).to_owned(),
                module: module_of(&n.key.qualified_name).to_owned(),
                is_method: n.key.kind == CodeNodeKind::Method,
            })
            .collect();

        for pending in std::mem::take(&mut self.pending_calls) {
            let caller_module = module_of(&self.nodes[pending.from].key.qualified_name).to_owned();
            let target = resolve_target(&callables, &pending, &caller_module)
                .unwrap_or_else(|| self.reference_node(&pending.written));
            self.edges.push(BuiltEdge {
                from: pending.from,
                to: target,
                relation: CodeRelation::Calls,
                confidence: SYNTAX_CALL_CONFIDENCE,
                evidence_kind: EvidenceKind::SyntaxInferred,
                site_start: pending.site_start,
                site_end: pending.site_end,
            });
        }
    }

    fn reference_node(&mut self, written: &str) -> usize {
        self.add_node(
            self.make_key(written.to_owned(), CodeNodeKind::ExternalDependency, None),
            false,
        )
    }

    /// The normalized-signature content hash for a fn/method (everything before
    /// the body, whitespace-collapsed). Independent of the body and of file
    /// position, so it is stable across edits that don't change the signature.
    fn signature_hash(&self, node: Node) -> ContentHash {
        let end = node
            .child_by_field_name("body")
            .map_or_else(|| node.end_byte(), |b| b.start_byte());
        self.signature_hash_between(node.start_byte(), end)
    }

    /// The same hash over an explicit byte range, for a declaration whose body is
    /// not its own `body` field — a TypeScript `const f = () => …`, where the
    /// signature spans the declarator and the body belongs to the arrow.
    fn signature_hash_between(&self, start: usize, end: usize) -> ContentHash {
        let raw = self.source.get(start..end).unwrap_or("");
        // Trim whatever opens the body in this language: `{` (Rust/ECMA), `;` (a
        // signature-only item), `:` (Python), `=>` (an arrow function).
        let mut cleaned = raw.trim();
        loop {
            let trimmed = cleaned
                .strip_suffix("=>")
                .or_else(|| cleaned.strip_suffix(['{', ';', ':']))
                .map(str::trim_end);
            match trimmed {
                Some(next) if next.len() < cleaned.len() => cleaned = next,
                _ => break,
            }
        }
        let normalized = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
        ContentHash(hex::encode(Sha256::digest(normalized.as_bytes())))
    }
}

/// An owned callable node, indexed for within-file call resolution.
struct Callable {
    index: usize,
    simple: String,
    module: String,
    is_method: bool,
}

/// Pick the local node a call resolves to, or `None` for an external callee.
fn resolve_target(
    callables: &[Callable],
    pending: &PendingCall,
    caller_module: &str,
) -> Option<usize> {
    if pending.is_method {
        // Receiver type is unknown at the syntax layer: only resolve when exactly
        // one owned method carries the name, else treat as external.
        let mut methods = callables
            .iter()
            .filter(|c| c.is_method && c.simple == pending.simple);
        let first = methods.next()?;
        return methods.next().is_none().then_some(first.index);
    }
    // Plain/path call: prefer a same-module definition, else any name match.
    let matches = callables.iter().filter(|c| c.simple == pending.simple);
    let mut fallback = None;
    for candidate in matches {
        if candidate.module == caller_module {
            return Some(candidate.index);
        }
        fallback.get_or_insert(candidate.index);
    }
    fallback
}

// --------------------------------------------------------------------------
// Small pure helpers
// --------------------------------------------------------------------------

/// `prefix<sep>segment` in `language`'s separator; `segment` alone when there is
/// no prefix. A prefix that already ends in the separator (a Python relative
/// import — `from . import x`) is concatenated rather than doubled.
fn join_qualified(prefix: &str, language: Language, segment: &str) -> String {
    let separator = language.separator();
    if prefix.is_empty() {
        segment.to_owned()
    } else if prefix.ends_with(separator) {
        format!("{prefix}{segment}")
    } else {
        format!("{prefix}{separator}{segment}")
    }
}

/// Split a qualified name into `(module_prefix, simple_name)`, accepting either
/// separator the graph stores — `::` (Rust) or `.` (Python/TypeScript).
///
/// No language parameter, because no language's names contain both. The one
/// ambiguity is a **file** node, whose qualified name is a path (`src/main.py`),
/// where the trailing `.py` is an extension and not a symbol; a name carrying a
/// path separator is therefore never split. Callers pass symbol names — both
/// `repomap` sites filter to Type/Trait/Function/Method/Constant/Test/Module
/// first — so this guard is belt and braces.
fn split_qualified(qualified: &str) -> Option<(&str, &str)> {
    if let Some(at) = qualified.rfind("::") {
        return Some((&qualified[..at], &qualified[at + 2..]));
    }
    if qualified.contains('/') || qualified.contains('\\') {
        return None;
    }
    let at = qualified.rfind('.')?;
    let (prefix, tail) = (&qualified[..at], &qualified[at + 1..]);
    let tail_is_identifier = tail
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_');
    (!prefix.is_empty() && tail_is_identifier).then_some((prefix, tail))
}

/// The last segment of a qualified name (its simple name).
pub(crate) fn last_segment(qualified: &str) -> &str {
    split_qualified(qualified).map_or(qualified, |(_, simple)| simple)
}

/// The module prefix of a qualified name (everything before the last segment);
/// the empty string for a root-level symbol.
pub(crate) fn module_of(qualified: &str) -> &str {
    split_qualified(qualified).map_or("", |(prefix, _)| prefix)
}

/// Whether `path` is a test file by its ecosystem's naming convention.
///
/// Rust marks a test with `#[test]`, which the walk can read. Python and
/// JavaScript do not mark the *declaration* at all — pytest collects `test_*`
/// from `test_*.py`, and Jest/Vitest collect from `*.test.*` / `*.spec.*` — so
/// the file name is the only syntax-layer signal there is. Without it
/// `graph.tests_covering` can only ever answer for Rust.
fn is_test_path(language: Language, path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let file = normalized.rsplit('/').next().unwrap_or(&normalized);
    match language {
        // `#[test]`/`#[cfg(test)]` is authoritative for Rust; the path adds nothing.
        Language::Rust => false,
        Language::Python => {
            file.starts_with("test_")
                || file.ends_with("_test.py")
                || file.ends_with("_test.pyi")
                || normalized.split('/').any(|part| part == "tests")
        }
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            file.contains(".test.")
                || file.contains(".spec.")
                || normalized
                    .split('/')
                    .any(|part| part == "__tests__" || part == "__mocks__")
        }
    }
}

/// The bare type name of an `impl` self type (`Foo<T>` → `Foo`, `a::Bar` → `Bar`).
fn impl_type_name(text: &str) -> String {
    let base = text.split('<').next().unwrap_or(text).trim();
    last_segment(base).trim().to_owned()
}

/// Whether an attribute marks a test fn (`#[test]` or e.g. `#[tokio::test]`).
fn is_test_attr(attr: &str) -> bool {
    let inner = attr_inner(attr);
    inner == "test" || inner.ends_with("::test")
}

/// Whether an attribute is `#[cfg(test)]` (a test-only module gate).
fn is_cfg_test_attr(attr: &str) -> bool {
    let inner = attr_inner(attr);
    let predicate = inner
        .strip_prefix("cfg(")
        .and_then(|value| value.strip_suffix(')'))
        .or_else(|| {
            inner
                .strip_prefix("cfg_attr(")
                .and_then(|value| value.split_once(',').map(|(predicate, _)| predicate))
        });
    predicate.is_some_and(cfg_predicate_requires_test)
}

fn cfg_predicate_requires_test(predicate: &str) -> bool {
    let predicate: String = predicate.chars().filter(|c| !c.is_whitespace()).collect();
    if predicate == "test" {
        return true;
    }
    if predicate.starts_with("not(") {
        return false;
    }
    (predicate.starts_with("any(") || predicate.starts_with("all("))
        && predicate
            .trim_start_matches("any(")
            .trim_start_matches("all(")
            .trim_end_matches(')')
            .split(',')
            .any(cfg_predicate_requires_test)
}

/// The text inside an attribute's brackets: `#[cfg(test)]` → `cfg(test)`.
fn attr_inner(attr: &str) -> &str {
    attr.trim()
        .trim_start_matches("#!")
        .trim_start_matches('#')
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim()
}
