//! The syntax-layer code graph (Chapter 07, STEP 2.5).
//!
//! Tree-sitter parses a Rust source file into the durable graph: the important
//! symbols (files, modules, types, traits, functions, methods, constants, and
//! tests — never local variables) as [`CodeNode`]s keyed by a position-stable
//! [`SymbolKey`], and the `Contains` / `Defines` / `Imports` / `Calls`-as-written
//! relations between them as evidence-backed [`CodeEdge`]s.
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

use std::collections::HashMap;
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
    GitRevision, LanguageId, SymbolKey, SYNTAX_CALL_CONFIDENCE,
};

/// The IANA media type recorded on a file's descriptive evidence artifact.
const RUST_MEDIA_TYPE: &str = "text/x-rust";

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

/// Retire a repository's entire code graph — every edge, then every node.
///
/// A per-file [`upsert_file_graph`] retires the symbols *its own file* no longer
/// defines (nodes are keyed by `source_path`), but it never sees a file that was
/// deleted outright — nothing reparses it, so its nodes would linger. The
/// Phase-2 pipeline rebuilds the graph with a full working-tree scan on each
/// startup (there is no live per-file watcher yet), and wiping the repository
/// first is how a *removed file's* symbols stop lingering in the graph (and in
/// the repository map, which reads every node for the repository). Code nodes are
/// a derived, regenerable projection — nothing durable references their ids — so
/// discarding and rebuilding them is safe.
pub async fn clear_repository(
    pool: &SqlitePool,
    repository: RepositoryId,
) -> Result<(), CodeGraphError> {
    let repo = repository.to_string();
    sqlx::query(
        "DELETE FROM code_edges WHERE from_node IN (SELECT id FROM code_nodes WHERE repository = ?) \
         OR to_node IN (SELECT id FROM code_nodes WHERE repository = ?)",
    )
    .bind(&repo)
    .bind(&repo)
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM code_nodes WHERE repository = ?")
        .bind(&repo)
        .execute(pool)
        .await?;
    Ok(())
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

    // 2. Replace this file's edges. Every edge produced by parsing a file has a
    //    `from_node` that is one of the file's own nodes (they all carry this
    //    `source_path`), so deleting by that set removes exactly the previous
    //    parse's edges — including edges out of a symbol this reparse drops — and
    //    nothing from any other file.
    let removed = sqlx::query(
        "DELETE FROM code_edges WHERE from_node IN \
         (SELECT id FROM code_nodes WHERE repository = ? AND source_path = ?)",
    )
    .bind(repository.to_string())
    .bind(path)
    .execute(&mut *tx)
    .await?;
    let removed_edges = removed.rows_affected();

    // 2b. Retire any symbol this file no longer defines (issue #6 item 4). Prior
    //     nodes for this `source_path` that this parse did not re-see are now
    //     edge-free (step 2 removed their outgoing edges, and every edge into
    //     them came from this same file), so a single-file reparse drops removed
    //     functions/types without waiting for a whole-repository `clear`.
    if !ids.is_empty() {
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "DELETE FROM code_nodes WHERE repository = ? AND source_path = ? \
             AND id NOT IN ({placeholders})"
        );
        let mut query = sqlx::query(&sql).bind(repository.to_string()).bind(path);
        for id in &ids {
            query = query.bind(id.to_string());
        }
        query.execute(&mut *tx).await?;
    }

    // 3. Insert the fresh edges, each carrying its descriptive evidence ref.
    let mut edge_records = Vec::with_capacity(built.edges.len());
    for edge in &built.edges {
        let from = ids[edge.from];
        let to = ids[edge.to];
        let evidence = EvidenceRef::Artifact {
            artifact: built.file_artifact.clone(),
            source_path: Some(format!("{path}#{}-{}", edge.site_start, edge.site_end)),
        };
        let evidence_json = serde_json::to_string(&evidence)?;
        sqlx::query(
            "INSERT INTO code_edges \
             (id, from_node, to_node, relation, confidence, evidence_kind, evidence_artifact, \
              revision, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(from.to_string())
        .bind(to.to_string())
        .bind(scalar(&edge.relation))
        .bind(f64::from(edge.confidence))
        .bind(scalar(&edge.evidence_kind))
        .bind(&evidence_json)
        .bind(&revision.0)
        .bind(&created_at)
        .execute(&mut *tx)
        .await?;
        edge_records.push(CodeEdge {
            from,
            to,
            relation: edge.relation,
            confidence: edge.confidence,
            evidence_kind: edge.evidence_kind,
            evidence: Some(evidence),
            revision: revision.clone(),
        });
    }

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

/// Read back every node for `repository`, oldest first.
pub async fn nodes(
    pool: &SqlitePool,
    repository: RepositoryId,
) -> Result<Vec<CodeNode>, CodeGraphError> {
    let rows: Vec<NodeRow> = sqlx::query_as(
        "SELECT id, language, package, source_path, qualified_name, kind, signature_hash, revision \
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

// --------------------------------------------------------------------------
// Incremental pipeline — filesystem watcher (minimal)
// --------------------------------------------------------------------------

/// Arm a recursive filesystem watcher over `root`, forwarding raw notify events
/// to `handler`. This is intentionally minimal: it does not itself reparse — a
/// caller debounces the events, applies the ignore/generated-file policy, and
/// calls [`upsert_file_graph`] per changed file to produce a [`GraphDelta`].
///
/// The returned watcher owns its own background thread and stops when dropped;
/// tests never start one (no background threads in tests).
pub fn watch<F>(root: &Path, handler: F) -> Result<notify::RecommendedWatcher, CodeGraphError>
where
    F: FnMut(notify::Result<notify::Event>) + Send + 'static,
{
    let mut watcher = notify::recommended_watcher(handler)?;
    notify::Watcher::watch(&mut watcher, root, notify::RecursiveMode::Recursive)?;
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
    /// The repo-relative path being parsed; stamped onto every node's key so a
    /// file's symbols are identified independently of any other file's.
    path: &'a str,
    source: &'a str,
    nodes: Vec<BuiltNode>,
    edges: Vec<BuiltEdge>,
    pending_calls: Vec<PendingCall>,
    /// Dedup by `SymbolKey::stable_key()` so repeated imports/calls reuse a node.
    index: HashMap<String, usize>,
}

/// Parse `source` into a [`BuiltGraph`]. Deterministic: identical bytes always
/// produce identical nodes, edges, and evidence.
fn build_file_graph(
    repository: RepositoryId,
    path: &str,
    source: &str,
) -> Result<BuiltGraph, CodeGraphError> {
    let digest = Sha256::digest(source.as_bytes());
    let file_artifact = ArtifactRef {
        id: ArtifactId(Uuid::from_slice(&digest[..16])?),
        media_type: RUST_MEDIA_TYPE.to_owned(),
        byte_length: source.len() as u64,
        sha256: hex::encode(digest),
        sensitivity: DataClassification::Internal,
    };

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|e| CodeGraphError::Parse(e.to_string()))?;
    let tree = parser
        .parse(source.as_bytes(), None)
        .ok_or_else(|| CodeGraphError::Parse("tree-sitter returned no tree".to_owned()))?;

    let mut builder = Builder {
        repository,
        path,
        source,
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
    builder.visit_item_list(tree.root_node(), &root_ctx);
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
            language: LanguageId("rust".to_owned()),
            package: None,
            source_path: self.path.to_owned(),
            qualified_name,
            kind,
            signature_hash,
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

    /// Iterate the named children of an item list (`source_file` /
    /// `declaration_list`), attaching each pending attribute run to the item that
    /// follows it, and dispatch each item to its handler.
    fn visit_item_list(&mut self, list: Node, ctx: &Ctx) {
        if ctx.depth >= MAX_PARSE_DEPTH {
            return; // graceful truncation, never a stack overflow (see the const)
        }
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
        let qualified = join_scope(&ctx.scope_path, &name);
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
        let qualified = join_scope(&ctx.scope_path, &name);
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
            self.visit_item_list(body, &mod_ctx);
        }
    }

    fn handle_type(&mut self, node: Node, ctx: &Ctx) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let qualified = join_scope(&ctx.scope_path, &name);
        let idx = self.add_node(self.make_key(qualified, CodeNodeKind::Type, None), true);
        let span = (node.start_byte(), node.end_byte());
        self.add_edge(ctx.container, idx, CodeRelation::Contains, span);
        self.add_edge(ctx.definer, idx, CodeRelation::Defines, span);
    }

    fn handle_trait(&mut self, node: Node, ctx: &Ctx) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let qualified = join_scope(&ctx.scope_path, &name);
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
            self.visit_item_list(body, &trait_ctx);
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
            self.visit_item_list(body, &impl_ctx);
        }
    }

    fn handle_const(&mut self, node: Node, ctx: &Ctx) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let qualified = join_scope(&ctx.scope_path, &name);
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
            let idx = self.add_node(
                self.make_key(path, CodeNodeKind::ExternalDependency, None),
                false,
            );
            self.add_edge(ctx.container, idx, CodeRelation::Imports, span);
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
                let next = join_path(prefix, &path);
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
                out.push(join_path(prefix, &path));
            }
            _ => out.push(join_path(prefix, &self.text(node))),
        }
    }

    /// Descend a function body recording every call expression against the
    /// enclosing function. Nested `function_item`s are skipped (their calls
    /// belong to them, not the outer fn); closures are descended into.
    fn collect_calls(&mut self, node: Node, ctx: &Ctx, depth: usize) {
        if depth >= MAX_PARSE_DEPTH {
            return; // graceful truncation, never a stack overflow (see the const)
        }
        let children: Vec<Node> = node.named_children(&mut node.walk()).collect();
        for child in children {
            if child.kind() == "function_item" {
                continue;
            }
            if child.kind() == "call_expression" {
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
    /// when the callee is not a plain name/path/method (e.g. a call on a call).
    fn callee_name(&self, function: Node) -> Option<(String, String, bool)> {
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
                self.callee_name(inner)
            }
            _ => None,
        }
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
        let raw = self.source.get(node.start_byte()..end).unwrap_or("");
        let cleaned = raw.trim().trim_end_matches([';', '{']).trim();
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

fn join_scope(scope: &[String], name: &str) -> String {
    if scope.is_empty() {
        name.to_owned()
    } else {
        format!("{}::{}", scope.join("::"), name)
    }
}

fn join_path(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_owned()
    } else {
        format!("{prefix}::{segment}")
    }
}

/// The last `::`-segment of a qualified name (its simple name).
pub(crate) fn last_segment(qualified: &str) -> &str {
    qualified.rsplit("::").next().unwrap_or(qualified)
}

/// The module prefix of a qualified name (everything before the last segment);
/// the empty string for a crate-root symbol.
pub(crate) fn module_of(qualified: &str) -> &str {
    qualified.rsplit_once("::").map_or("", |(prefix, _)| prefix)
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
    inner.starts_with("cfg") && inner.contains("test")
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
