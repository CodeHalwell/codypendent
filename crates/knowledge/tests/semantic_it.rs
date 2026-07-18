//! STEP 4.5 semantic layer + revision-aware queries: an LSP-resolved edge
//! supersedes its syntax-inferred counterpart; blast-radius/callers/tests-covering
//! walk a known call chain; changed_between detects signature changes; the
//! LanguageAdapter parses and degrades to syntax-only without a language server.

use codypendent_knowledge::adapter::{
    on_path, LanguageAdapter, ParseInput, RustAdapter, ScriptAdapter, SemanticCapability, Workspace,
};
use codypendent_knowledge::codegraph::{
    self, blast_radius, callers_of, changed_between, tests_covering, SemanticEdge, SymbolSnapshot,
};
use codypendent_knowledge::types::{
    CodeNode, CodeNodeKind, CodeRelation, EvidenceKind, LSP_RESOLVED_CONFIDENCE,
};
use codypendent_knowledge::{db, GitRevision};
use codypendent_protocol::RepositoryId;

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

/// A single-file call chain: `driver` → `tick` → `compute`.
const CHAIN: &str = r#"
pub fn compute(x: u32) -> u32 { x + 1 }
pub fn tick() -> u32 { compute(1) }
pub fn driver() -> u32 { tick() }
"#;

fn key_of<'a>(nodes: &'a [CodeNode], name: &str) -> &'a CodeNode {
    nodes
        .iter()
        .find(|n| n.key.qualified_name == name)
        .unwrap_or_else(|| panic!("no node named {name}"))
}

#[tokio::test]
async fn callers_and_blast_radius_walk_the_chain() {
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    let rev = GitRevision("rev1".into());
    codegraph::upsert_file_graph(&pool, repo, &rev, "src/lib.rs", CHAIN)
        .await
        .unwrap();
    let nodes = codegraph::nodes(&pool, repo).await.unwrap();
    let compute_key = key_of(&nodes, "compute").key.stable_key();

    // Direct callers of `compute` are exactly `tick`.
    let direct = callers_of(&pool, repo, &compute_key).await.unwrap();
    assert_eq!(direct.len(), 1);
    assert_eq!(direct[0].key.qualified_name, "tick");

    // Blast radius grows with depth: {tick} at 1, {tick, driver} at 2.
    let r1 = blast_radius(&pool, repo, &compute_key, 1).await.unwrap();
    assert_eq!(r1.len(), 1);
    let r2 = blast_radius(&pool, repo, &compute_key, 2).await.unwrap();
    let names: Vec<&str> = r2.iter().map(|n| n.key.qualified_name.as_str()).collect();
    assert!(
        names.contains(&"tick") && names.contains(&"driver"),
        "{names:?}"
    );
}

#[tokio::test]
async fn lsp_edge_supersedes_the_syntax_edge() {
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    let rev = GitRevision("rev1".into());
    codegraph::upsert_file_graph(&pool, repo, &rev, "src/lib.rs", CHAIN)
        .await
        .unwrap();
    let nodes = codegraph::nodes(&pool, repo).await.unwrap();
    let tick = key_of(&nodes, "tick");
    let compute = key_of(&nodes, "compute");

    // Before: the syntax layer inferred a low-confidence tick → compute call.
    let before: Vec<_> = codegraph::edges(&pool, repo)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.from == tick.id && e.to == compute.id)
        .collect();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].evidence_kind, EvidenceKind::SyntaxInferred);

    // Fold in the LSP-resolved edge for the same (from, to, relation).
    let (applied, skipped) = codegraph::upsert_semantic_edges(
        &pool,
        repo,
        &rev,
        &[SemanticEdge {
            from_symbol_key: tick.key.stable_key(),
            to_symbol_key: compute.key.stable_key(),
            relation: CodeRelation::Calls,
            evidence_kind: EvidenceKind::LspResolved,
            confidence: LSP_RESOLVED_CONFIDENCE,
            evidence: None,
        }],
    )
    .await
    .unwrap();
    assert_eq!((applied, skipped), (1, 0));

    // After: exactly one tick → compute edge remains — the resolved one, at LSP
    // confidence. The syntax edge was superseded, not duplicated.
    let after: Vec<_> = codegraph::edges(&pool, repo)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.from == tick.id && e.to == compute.id)
        .collect();
    assert_eq!(after.len(), 1, "superseded, not duplicated");
    assert_eq!(after[0].evidence_kind, EvidenceKind::LspResolved);
    assert!((after[0].confidence - LSP_RESOLVED_CONFIDENCE).abs() < f32::EPSILON);
}

#[tokio::test]
async fn semantic_edge_with_missing_endpoint_is_skipped() {
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    let rev = GitRevision("rev1".into());
    codegraph::upsert_file_graph(&pool, repo, &rev, "src/lib.rs", CHAIN)
        .await
        .unwrap();
    let (applied, skipped) = codegraph::upsert_semantic_edges(
        &pool,
        repo,
        &rev,
        &[SemanticEdge {
            from_symbol_key: "does|not::exist#Function@".into(),
            to_symbol_key: "also|missing#Function@".into(),
            relation: CodeRelation::Calls,
            evidence_kind: EvidenceKind::LspResolved,
            confidence: LSP_RESOLVED_CONFIDENCE,
            evidence: None,
        }],
    )
    .await
    .unwrap();
    assert_eq!((applied, skipped), (0, 1));
}

#[tokio::test]
async fn tests_covering_follows_a_resolved_cross_file_edge() {
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    let rev = GitRevision("rev1".into());
    // The implementation and the test live in different files; the syntax layer
    // cannot link them, but an LSP-resolved edge can.
    codegraph::upsert_file_graph(
        &pool,
        repo,
        &rev,
        "src/lib.rs",
        "pub fn charge() -> u32 { 0 }",
    )
    .await
    .unwrap();
    codegraph::upsert_file_graph(
        &pool,
        repo,
        &rev,
        "tests/charge.rs",
        "#[test]\nfn charge_works() { assert_eq!(0, 0); }",
    )
    .await
    .unwrap();
    let nodes = codegraph::nodes(&pool, repo).await.unwrap();
    let charge = key_of(&nodes, "charge");
    let test = key_of(&nodes, "charge_works");
    assert_eq!(test.key.kind, CodeNodeKind::Test);

    codegraph::upsert_semantic_edges(
        &pool,
        repo,
        &rev,
        &[SemanticEdge {
            from_symbol_key: test.key.stable_key(),
            to_symbol_key: charge.key.stable_key(),
            relation: CodeRelation::Calls,
            evidence_kind: EvidenceKind::LspResolved,
            confidence: LSP_RESOLVED_CONFIDENCE,
            evidence: None,
        }],
    )
    .await
    .unwrap();

    let covering = tests_covering(&pool, repo, "src/lib.rs", 3).await.unwrap();
    assert_eq!(covering.len(), 1);
    assert_eq!(covering[0].key.qualified_name, "charge_works");
}

#[test]
fn changed_between_detects_added_removed_and_modified() {
    let sym = |name: &str, sig: Option<&str>| SymbolSnapshot {
        qualified_name: name.into(),
        kind: CodeNodeKind::Function,
        source_path: "src/lib.rs".into(),
        signature_hash: sig.map(str::to_string),
    };
    let before = vec![
        sym("stable", Some("a")),
        sym("gone", Some("b")),
        sym("changed", Some("c")),
    ];
    let after = vec![
        sym("stable", Some("a")),
        sym("changed", Some("c2")),
        sym("fresh", Some("d")),
    ];

    let delta = changed_between(&before, &after);
    assert_eq!(
        delta
            .added
            .iter()
            .map(|s| s.qualified_name.as_str())
            .collect::<Vec<_>>(),
        ["fresh"]
    );
    assert_eq!(
        delta
            .removed
            .iter()
            .map(|s| s.qualified_name.as_str())
            .collect::<Vec<_>>(),
        ["gone"]
    );
    assert_eq!(delta.modified.len(), 1);
    assert_eq!(delta.modified[0].1.qualified_name, "changed");
}

#[tokio::test]
async fn rust_adapter_parses_and_degrades_without_a_language_server() {
    let adapter = RustAdapter;
    let out = adapter
        .parse(ParseInput {
            path: "src/lib.rs".into(),
            source: CHAIN.into(),
        })
        .await
        .unwrap();
    let names: Vec<&str> = out
        .symbols
        .iter()
        .map(|s| s.qualified_name.as_str())
        .collect();
    assert!(names.contains(&"compute") && names.contains(&"tick") && names.contains(&"driver"));

    // The syntax parse works regardless of tooling (graceful degradation). The
    // reported capability is exactly whether rust-analyzer is on PATH — LSP when
    // present, SyntaxOnly when absent; never a failure. A binary that cannot be
    // installed anywhere is always absent.
    let expected = if on_path("rust-analyzer") {
        SemanticCapability::LspResolved
    } else {
        SemanticCapability::SyntaxOnly
    };
    assert_eq!(adapter.capability(), expected);
    assert!(!on_path("codypendent-no-such-language-server"));
}

#[tokio::test]
async fn thin_python_and_typescript_adapters_scan_declarations() {
    let py = ScriptAdapter::python();
    let out = py
        .parse(ParseInput {
            path: "m.py".into(),
            source: "def charge(x):\n    return x\n\nclass Wallet:\n    pass\n".into(),
        })
        .await
        .unwrap();
    let names: Vec<&str> = out
        .symbols
        .iter()
        .map(|s| s.qualified_name.as_str())
        .collect();
    assert_eq!(names, ["charge", "Wallet"]);
    // Capability reflects whether pyright is present; the syntax scan works either
    // way (graceful degradation).
    let expected = if on_path("pyright") {
        SemanticCapability::LspResolved
    } else {
        SemanticCapability::SyntaxOnly
    };
    assert_eq!(py.capability(), expected);

    let ts = ScriptAdapter::typescript();
    let out = ts
        .parse(ParseInput {
            path: "m.ts".into(),
            source: "export function charge(x: number) {}\nexport class Wallet {}\n".into(),
        })
        .await
        .unwrap();
    let names: Vec<&str> = out
        .symbols
        .iter()
        .map(|s| s.qualified_name.as_str())
        .collect();
    assert_eq!(names, ["charge", "Wallet"]);
}

#[tokio::test]
async fn hierarchical_map_folds_bottom_up_with_evidence() {
    use codypendent_knowledge::repomap::{hierarchical_map, MapLevel};
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    let rev = GitRevision("rev1".into());
    codegraph::upsert_file_graph(&pool, repo, &rev, "src/lib.rs", CHAIN)
        .await
        .unwrap();

    let map = hierarchical_map(&pool, repo).await.unwrap();
    assert_eq!(map.level, MapLevel::Workspace);
    // workspace → package → module, each recording the evidence beneath it.
    assert_eq!(map.evidence.symbol_count, 3);
    assert_eq!(map.evidence.revision.as_deref(), Some("rev1"));
    let package = &map.children[0];
    assert_eq!(package.level, MapLevel::Package);
    assert_eq!(package.evidence.symbol_count, 3);
    let module = &package.children[0];
    assert_eq!(module.level, MapLevel::Module);
    assert_eq!(module.evidence.symbol_count, 3);
    let symbols: Vec<&str> = module.children.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(symbols, ["compute", "driver", "tick"]);
}

#[tokio::test]
async fn rust_adapter_reads_cargo_metadata() {
    // A minimal crate in a temp dir: `cargo metadata --no-deps` needs no network.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"fixture-pkg\"\nversion = \"0.3.1\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "pub fn f() {}\n").unwrap();

    let adapter = RustAdapter;
    let meta = adapter
        .build_metadata(&Workspace::new(tmp.path()))
        .await
        .unwrap();
    assert!(meta
        .packages
        .iter()
        .any(|p| p.name == "fixture-pkg" && p.version == "0.3.1"));
}
