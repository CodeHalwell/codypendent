//! STEP 2.5 code graph: tree-sitter parse → durable nodes/edges, stable symbol
//! identity across file rename, incremental reparse == full reparse, and the
//! repository map render.

use std::collections::HashMap;

use codypendent_knowledge::codegraph::{self, CodeGraphError};
use codypendent_knowledge::repomap::repository_map;
use codypendent_knowledge::types::{CodeNode, CodeNodeKind, CodeRelation, EvidenceKind};
use codypendent_knowledge::{db, outbox, GitRevision};
use codypendent_protocol::RepositoryId;

/// A small fixture crate exercising every extracted node kind and edge relation:
/// imports, a constant, a struct, a trait, an impl with methods, a free function
/// called from a method, a nested module, and a `#[cfg(test)]` module whose
/// `#[test]` fn calls back into the API.
const FIXTURE: &str = r#"
use std::fmt;
use crate::util::{helper, Widget as W};

pub const MAX: u32 = 10;

pub struct Engine {
    count: u32,
}

pub trait Runnable {
    fn run(&self);
}

impl Engine {
    pub fn new() -> Engine {
        Engine { count: 0 }
    }

    pub fn tick(&self) -> u32 {
        compute(self.count)
    }
}

pub fn compute(seed: u32) -> u32 {
    seed + 1
}

mod inner {
    pub fn deep() -> u32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_ticks() {
        let e = Engine::new();
        let _ = e.tick();
        let _ = compute(1);
    }
}
"#;

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

fn rev() -> GitRevision {
    GitRevision("rev-1".to_owned())
}

fn has_node(nodes: &[CodeNode], qualified: &str, kind: CodeNodeKind) -> bool {
    nodes
        .iter()
        .any(|n| n.key.qualified_name == qualified && n.key.kind == kind)
}

/// Build a `(qualified_name, relation, qualified_name)` view of the edges, so
/// they can be asserted without knowing the generated node ids.
fn edge_triples(
    nodes: &[CodeNode],
    edges: &[codypendent_knowledge::CodeEdge],
) -> Vec<(String, CodeRelation, String)> {
    let by_id: HashMap<_, _> = nodes
        .iter()
        .map(|n| (n.id, n.key.qualified_name.clone()))
        .collect();
    edges
        .iter()
        .map(|e| {
            (
                by_id.get(&e.from).cloned().unwrap_or_default(),
                e.relation,
                by_id.get(&e.to).cloned().unwrap_or_default(),
            )
        })
        .collect()
}

fn has_edge(
    triples: &[(String, CodeRelation, String)],
    from: &str,
    rel: CodeRelation,
    to: &str,
) -> bool {
    triples
        .iter()
        .any(|(f, r, t)| f == from && *r == rel && t == to)
}

#[tokio::test]
async fn parses_expected_nodes_and_edges() {
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    let path = "src/engine.rs";

    let delta = codegraph::upsert_file_graph(&pool, repo, &rev(), path, FIXTURE)
        .await
        .unwrap();

    let nodes = codegraph::nodes(&pool, repo).await.unwrap();

    // Every extracted node kind is present, keyed by qualified name.
    assert!(has_node(&nodes, path, CodeNodeKind::File));
    assert!(has_node(&nodes, "MAX", CodeNodeKind::Constant));
    assert!(has_node(&nodes, "Engine", CodeNodeKind::Type));
    assert!(has_node(&nodes, "Runnable", CodeNodeKind::TraitOrInterface));
    assert!(has_node(&nodes, "Runnable::run", CodeNodeKind::Method));
    assert!(has_node(&nodes, "Engine::new", CodeNodeKind::Method));
    assert!(has_node(&nodes, "Engine::tick", CodeNodeKind::Method));
    assert!(has_node(&nodes, "compute", CodeNodeKind::Function));
    assert!(has_node(&nodes, "inner", CodeNodeKind::Module));
    assert!(has_node(&nodes, "inner::deep", CodeNodeKind::Function));
    assert!(has_node(&nodes, "tests", CodeNodeKind::Module));
    assert!(has_node(&nodes, "tests::engine_ticks", CodeNodeKind::Test));

    // Imports become ExternalDependency reference nodes named by the use path.
    assert!(has_node(
        &nodes,
        "std::fmt",
        CodeNodeKind::ExternalDependency
    ));
    assert!(has_node(
        &nodes,
        "crate::util::helper",
        CodeNodeKind::ExternalDependency
    ));
    assert!(has_node(
        &nodes,
        "crate::util::Widget",
        CodeNodeKind::ExternalDependency
    ));

    let edges = codegraph::edges(&pool, repo).await.unwrap();
    let triples = edge_triples(&nodes, &edges);

    // Contains: file → item, and module → nested item.
    assert!(has_edge(&triples, path, CodeRelation::Contains, "compute"));
    assert!(has_edge(&triples, path, CodeRelation::Contains, "inner"));
    assert!(has_edge(
        &triples,
        "inner",
        CodeRelation::Contains,
        "inner::deep"
    ));
    assert!(has_edge(
        &triples,
        "tests",
        CodeRelation::Contains,
        "tests::engine_ticks"
    ));

    // Defines: the definer (file/module/trait) → item.
    assert!(has_edge(&triples, path, CodeRelation::Defines, "Engine"));
    assert!(has_edge(&triples, path, CodeRelation::Defines, "compute"));
    assert!(has_edge(
        &triples,
        "Runnable",
        CodeRelation::Defines,
        "Runnable::run"
    ));

    // Imports: file → the imported path.
    assert!(has_edge(&triples, path, CodeRelation::Imports, "std::fmt"));

    // Calls-as-written, resolved within the file to real owned nodes.
    assert!(has_edge(
        &triples,
        "Engine::tick",
        CodeRelation::Calls,
        "compute"
    ));
    assert!(has_edge(
        &triples,
        "tests::engine_ticks",
        CodeRelation::Calls,
        "Engine::tick"
    ));
    assert!(has_edge(
        &triples,
        "tests::engine_ticks",
        CodeRelation::Calls,
        "Engine::new"
    ));

    // Call edges carry the Chapter 07 syntax-inferred confidence + evidence.
    let call = edges
        .iter()
        .find(|e| e.relation == CodeRelation::Calls)
        .expect("a Calls edge");
    assert!((call.confidence - 0.45).abs() < f32::EPSILON);
    assert_eq!(call.evidence_kind, EvidenceKind::SyntaxInferred);
    assert!(
        call.evidence.is_some(),
        "every edge carries an evidence ref"
    );

    // One SymbolChanged outbox event per durable node (the 12 owned symbols),
    // enqueued in the write tx. The synthesized import reference nodes are also
    // created but are not symbols, so they emit no event.
    let events = outbox::unprocessed(&pool, 1000).await.unwrap();
    assert!(events.iter().all(|e| e.event_kind == "symbol_changed"));
    assert_eq!(events.len(), 12, "one SymbolChanged per durable symbol");
    assert!(
        delta.created_node_ids.len() > events.len(),
        "reference nodes were created too, without events"
    );
}

#[tokio::test]
async fn symbol_identity_survives_file_rename() {
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();

    codegraph::upsert_file_graph(&pool, repo, &rev(), "src/engine.rs", FIXTURE)
        .await
        .unwrap();
    let before = codegraph::nodes(&pool, repo).await.unwrap();
    let compute_before = before
        .iter()
        .find(|n| n.key.qualified_name == "compute" && n.key.kind == CodeNodeKind::Function)
        .expect("compute node")
        .id;

    // Same symbols, different path: the durable symbols keep their id (SymbolKey
    // is position-independent), even though a fresh File node appears.
    codegraph::upsert_file_graph(&pool, repo, &rev(), "src/core/engine.rs", FIXTURE)
        .await
        .unwrap();
    let after = codegraph::nodes(&pool, repo).await.unwrap();
    let compute_after = after
        .iter()
        .find(|n| n.key.qualified_name == "compute" && n.key.kind == CodeNodeKind::Function)
        .expect("compute node")
        .id;

    assert_eq!(compute_before, compute_after, "identity survives rename");
    assert!(has_node(&after, "src/engine.rs", CodeNodeKind::File));
    assert!(has_node(&after, "src/core/engine.rs", CodeNodeKind::File));
}

/// A comparable, id-independent projection of a whole repository graph.
async fn projection(
    pool: &sqlx::SqlitePool,
    repo: RepositoryId,
) -> Result<(Vec<String>, Vec<String>), CodeGraphError> {
    let nodes = codegraph::nodes(pool, repo).await?;
    let edges = codegraph::edges(pool, repo).await?;
    let mut node_keys: Vec<String> = nodes
        .iter()
        .map(|n| format!("{}|{:?}", n.key.qualified_name, n.key.kind))
        .collect();
    node_keys.sort();
    let mut edge_keys: Vec<String> = edge_triples(&nodes, &edges)
        .into_iter()
        .map(|(f, r, t)| format!("{f}|{r:?}|{t}"))
        .collect();
    edge_keys.sort();
    Ok((node_keys, edge_keys))
}

#[tokio::test]
async fn incremental_reparse_equals_full_reparse() {
    // Full: one clean parse into a fresh database.
    let (_tmp_a, pool_a) = temp_pool().await;
    let repo = RepositoryId::new();
    codegraph::upsert_file_graph(&pool_a, repo, &rev(), "src/engine.rs", FIXTURE)
        .await
        .unwrap();
    let full = projection(&pool_a, repo).await.unwrap();

    // Incremental: parse the same file twice into another database.
    let (_tmp_b, pool_b) = temp_pool().await;
    codegraph::upsert_file_graph(&pool_b, repo, &rev(), "src/engine.rs", FIXTURE)
        .await
        .unwrap();
    let first_nodes = codegraph::nodes(&pool_b, repo).await.unwrap();
    let compute_first = first_nodes
        .iter()
        .find(|n| n.key.qualified_name == "compute")
        .unwrap()
        .id;

    let second = codegraph::upsert_file_graph(&pool_b, repo, &rev(), "src/engine.rs", FIXTURE)
        .await
        .unwrap();
    let incremental = projection(&pool_b, repo).await.unwrap();

    // The graphs are identical (same node set, same edge set).
    assert_eq!(full, incremental, "incremental delta equals full reparse");

    // The reparse replaced every edge and preserved node identity.
    assert_eq!(second.removed_edges as usize, second.edges.len());
    assert!(
        second.created_node_ids.is_empty(),
        "no new nodes on reparse"
    );
    let compute_second = codegraph::nodes(&pool_b, repo)
        .await
        .unwrap()
        .into_iter()
        .find(|n| n.key.qualified_name == "compute")
        .unwrap()
        .id;
    assert_eq!(compute_first, compute_second);
}

#[tokio::test]
async fn repository_map_renders_apis_and_tests() {
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    codegraph::upsert_file_graph(&pool, repo, &rev(), "src/engine.rs", FIXTURE)
        .await
        .unwrap();

    let map = repository_map(&pool, repo).await.unwrap();
    let rendered = map.render();

    // Public API surface is present.
    assert!(
        rendered.contains("compute"),
        "public fn in map:\n{rendered}"
    );
    assert!(
        rendered.contains("Engine"),
        "public type in map:\n{rendered}"
    );
    assert!(rendered.contains("MAX"), "public const in map:\n{rendered}");
    // Test names are present and labelled.
    assert!(
        rendered.contains("test engine_ticks"),
        "test name in map:\n{rendered}"
    );
    // The change surface slot renders (empty stub in v1).
    assert!(rendered.contains("change surface: (none)"));
}
