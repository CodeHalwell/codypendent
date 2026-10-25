//! Cross-file syntax links require explicit, unambiguous import bindings.

use codypendent_knowledge::codegraph::{self, ScanCoverage};
use codypendent_knowledge::{
    db, CodeEdge, CodeNode, CodeNodeKind, CodeRelation, EvidenceKind, EvidenceRef, GitRevision,
};
use codypendent_protocol::RepositoryId;

async fn fixture(
    files: &[(&str, &str)],
) -> (
    tempfile::TempDir,
    sqlx::SqlitePool,
    RepositoryId,
    Vec<CodeNode>,
    Vec<CodeEdge>,
) {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::open(&dir.path().join("graph.db")).await.unwrap();
    let repo = RepositoryId::new();
    codegraph::rebuild_repository(
        &pool,
        repo,
        &GitRevision("imports-1".into()),
        files.iter().copied(),
        ScanCoverage::Complete,
    )
    .await
    .unwrap();
    let nodes = codegraph::nodes(&pool, repo).await.unwrap();
    let edges = codegraph::edges(&pool, repo).await.unwrap();
    (dir, pool, repo, nodes, edges)
}

fn linked(
    nodes: &[CodeNode],
    edges: &[CodeEdge],
    from: (&str, &str),
    relation: CodeRelation,
    to: (&str, &str),
) -> bool {
    let find = |key: (&str, &str)| {
        nodes
            .iter()
            .find(|node| {
                node.key.source_path == key.0
                    && node.key.qualified_name == key.1
                    && node.key.kind != CodeNodeKind::ExternalDependency
            })
            .map(|node| node.id)
    };
    let (Some(from), Some(to)) = (find(from), find(to)) else {
        return false;
    };
    edges
        .iter()
        .any(|edge| edge.from == from && edge.to == to && edge.relation == relation)
}

#[tokio::test]
async fn python_imports_connect_models_inheritance_annotations_methods_and_callers() {
    let main = "from .models import Model as DataModel\nfrom .service import run as go\nimport pkg.service as service\nclass Derived(DataModel):\n    pass\ndef main(model: DataModel) -> DataModel:\n    go()\n    service.run()\n    DataModel.validate(model)\n    return model\n";
    let (_dir, pool, repo, nodes, edges) = fixture(&[
        ("pkg/main.py", main),
        (
            "pkg/models.py",
            "class Model:\n    def validate(self):\n        return True\n",
        ),
        ("pkg/service.py", "def run():\n    return 42\n"),
    ])
    .await;
    for (relation, from, to) in [
        (
            CodeRelation::Imports,
            ("pkg/main.py", "pkg/main.py"),
            ("pkg/models.py", "Model"),
        ),
        (
            CodeRelation::Imports,
            ("pkg/main.py", "pkg/main.py"),
            ("pkg/service.py", "pkg/service.py"),
        ),
        (
            CodeRelation::Extends,
            ("pkg/main.py", "Derived"),
            ("pkg/models.py", "Model"),
        ),
        (
            CodeRelation::References,
            ("pkg/main.py", "main"),
            ("pkg/models.py", "Model"),
        ),
        (
            CodeRelation::Calls,
            ("pkg/main.py", "main"),
            ("pkg/service.py", "run"),
        ),
        (
            CodeRelation::Calls,
            ("pkg/main.py", "main"),
            ("pkg/models.py", "Model.validate"),
        ),
    ] {
        assert!(
            linked(&nodes, &edges, from, relation, to),
            "missing {relation:?}: {from:?} -> {to:?}"
        );
    }
    let target = nodes
        .iter()
        .find(|node| {
            node.key.source_path == "pkg/service.py"
                && node.key.qualified_name == "run"
                && node.key.kind == CodeNodeKind::Function
        })
        .unwrap();
    let callers = codegraph::callers_of(&pool, repo, &target.key.stable_key())
        .await
        .unwrap();
    assert!(callers
        .iter()
        .any(|node| node.key.source_path == "pkg/main.py" && node.key.qualified_name == "main"));
    let call = edges
        .iter()
        .find(|edge| edge.to == target.id && edge.relation == CodeRelation::Calls)
        .unwrap();
    assert_eq!(call.evidence_kind, EvidenceKind::SyntaxInferred);
    let Some(EvidenceRef::Artifact {
        source_path: Some(location),
        artifact,
    }) = &call.evidence
    else {
        panic!("call-site evidence missing");
    };
    assert!(location.starts_with("pkg/main.py#"));
    assert_eq!(artifact.byte_length, main.len() as u64);
    assert!(
        nodes
            .iter()
            .all(|node| node.key.source_path != "pkg/main.py"
                || node.key.kind != CodeNodeKind::ExternalDependency),
        "resolved imports must not leave disconnected reference placeholders"
    );
}

#[tokio::test]
async fn python_from_package_import_module_resolves_without_guessing_a_bare_function() {
    let (_dir, _pool, _repo, nodes, edges) = fixture(&[
        (
            "pkg/main.py",
            "from . import service as svc\ndef main():\n    svc.run()\n",
        ),
        ("pkg/__init__.py", ""),
        ("pkg/service.py", "def run():\n    return 42\n"),
    ])
    .await;
    assert!(linked(
        &nodes,
        &edges,
        ("pkg/main.py", "pkg/main.py"),
        CodeRelation::Imports,
        ("pkg/service.py", "pkg/service.py")
    ));
    assert!(linked(
        &nodes,
        &edges,
        ("pkg/main.py", "main"),
        CodeRelation::Calls,
        ("pkg/service.py", "run")
    ));
}

#[tokio::test]
async fn generic_arguments_and_metaclasses_are_not_invented_as_base_classes() {
    let (_dir, _pool, _repo, nodes, edges) = fixture(&[
        ("pkg/main.py", "from .models import Base, Argument\nclass Derived(Base[Argument], metaclass=Argument):\n    pass\n"),
        ("pkg/models.py", "class Base:\n    pass\nclass Argument:\n    pass\n"),
        ("src/main.ts", "import { Base, Argument, Contract } from './models';\nexport class Derived extends Base<Argument> implements Contract {}\n"),
        ("src/models.ts", "export class Base<T> {}\nexport class Argument {}\nexport interface Contract {}\n"),
    ]).await;
    for (source, target) in [
        ("pkg/main.py", "pkg/models.py"),
        ("src/main.ts", "src/models.ts"),
    ] {
        assert!(linked(
            &nodes,
            &edges,
            (source, "Derived"),
            CodeRelation::Extends,
            (target, "Base")
        ));
        assert!(!linked(
            &nodes,
            &edges,
            (source, "Derived"),
            CodeRelation::Extends,
            (target, "Argument")
        ));
    }
    assert!(linked(
        &nodes,
        &edges,
        ("src/main.ts", "Derived"),
        CodeRelation::Implements,
        ("src/models.ts", "Contract")
    ));
    assert!(!linked(
        &nodes,
        &edges,
        ("src/main.ts", "Derived"),
        CodeRelation::Extends,
        ("src/models.ts", "Contract")
    ));
}

#[tokio::test]
async fn rust_explicit_crate_imports_connect_trait_model_and_cross_file_call() {
    let (_dir, _pool, _repo, nodes, edges) = fixture(&[
        ("src/lib.rs", "use crate::contracts::Contract;\nuse crate::service::run as go;\npub struct Model;\nimpl Contract for Model { fn check(&self) { go(); } }\n"),
        ("src/contracts.rs", "pub trait Contract { fn check(&self); }\n"),
        ("src/service.rs", "pub fn run() {}\n"),
    ]).await;
    assert!(linked(
        &nodes,
        &edges,
        ("src/lib.rs", "src/lib.rs"),
        CodeRelation::Imports,
        ("src/contracts.rs", "Contract")
    ));
    assert!(linked(
        &nodes,
        &edges,
        ("src/lib.rs", "Model"),
        CodeRelation::Implements,
        ("src/contracts.rs", "Contract")
    ));
    assert!(linked(
        &nodes,
        &edges,
        ("src/lib.rs", "Model::check"),
        CodeRelation::Calls,
        ("src/service.rs", "run")
    ));
}

#[tokio::test]
async fn typescript_named_aliases_connect_classes_types_and_methods() {
    let (_dir, _pool, _repo, nodes, edges) = fixture(&[
        ("src/main.ts", "import { Model as DataModel } from './models';\nimport { run as go } from './service';\nexport class Derived extends DataModel { render(input: DataModel): DataModel { go(); DataModel.validate(input); return input; } }\n"),
        ("src/models.ts", "export class Model { static validate(input: Model): boolean { return true; } }\n"),
        ("src/service.ts", "export function run(): void {}\n"),
    ]).await;
    for (relation, from, to) in [
        (
            CodeRelation::Imports,
            ("src/main.ts", "src/main.ts"),
            ("src/models.ts", "Model"),
        ),
        (
            CodeRelation::Extends,
            ("src/main.ts", "Derived"),
            ("src/models.ts", "Model"),
        ),
        (
            CodeRelation::References,
            ("src/main.ts", "Derived.render"),
            ("src/models.ts", "Model"),
        ),
        (
            CodeRelation::Calls,
            ("src/main.ts", "Derived.render"),
            ("src/service.ts", "run"),
        ),
        (
            CodeRelation::Calls,
            ("src/main.ts", "Derived.render"),
            ("src/models.ts", "Model.validate"),
        ),
    ] {
        assert!(
            linked(&nodes, &edges, from, relation, to),
            "missing {relation:?}: {from:?} -> {to:?}"
        );
    }
}

#[tokio::test]
async fn ambiguous_roots_shadowed_aliases_wildcards_and_foreign_repositories_do_not_resolve() {
    let (_dir, pool, repo, nodes, edges) = fixture(&[
        ("main.py", "import a as mod\nfrom a import work as shadowed\nfrom a import *\ndef main(shadowed):\n    mod.work()\n    shadowed()\n    work()\n"),
        ("a.py", "def work():\n    pass\n"),
        ("src/a.py", "def work():\n    pass\n"),
    ]).await;
    for target in ["a.py", "src/a.py"] {
        assert!(!linked(
            &nodes,
            &edges,
            ("main.py", "main"),
            CodeRelation::Calls,
            (target, "work")
        ));
    }
    let foreign = RepositoryId::new();
    codegraph::upsert_file_graph(
        &pool,
        foreign,
        &GitRevision("foreign".into()),
        "only.py",
        "def external():\n    pass\n",
    )
    .await
    .unwrap();
    codegraph::upsert_file_graph(
        &pool,
        repo,
        &GitRevision("imports-2".into()),
        "caller.py",
        "from only import external\ndef caller():\n    external()\n",
    )
    .await
    .unwrap();
    let foreign_ids: Vec<_> = codegraph::nodes(&pool, foreign)
        .await
        .unwrap()
        .into_iter()
        .map(|node| node.id)
        .collect();
    assert!(codegraph::edges(&pool, repo)
        .await
        .unwrap()
        .iter()
        .all(|edge| !foreign_ids.contains(&edge.to)));
}

#[tokio::test]
async fn incremental_caller_resolves_existing_target_and_rebuild_retires_deleted_targets() {
    let (_dir, pool, repo, _, _) = fixture(&[("pkg/service.py", "def run():\n    pass\n")]).await;
    let caller = "from .service import run\ndef main():\n    run()\n";
    codegraph::upsert_file_graph(
        &pool,
        repo,
        &GitRevision("imports-2".into()),
        "pkg/main.py",
        caller,
    )
    .await
    .unwrap();
    let nodes = codegraph::nodes(&pool, repo).await.unwrap();
    let edges = codegraph::edges(&pool, repo).await.unwrap();
    assert!(linked(
        &nodes,
        &edges,
        ("pkg/main.py", "main"),
        CodeRelation::Calls,
        ("pkg/service.py", "run")
    ));
    codegraph::rebuild_repository(
        &pool,
        repo,
        &GitRevision("imports-3".into()),
        [("pkg/main.py", caller)],
        ScanCoverage::Complete,
    )
    .await
    .unwrap();
    let nodes = codegraph::nodes(&pool, repo).await.unwrap();
    let edges = codegraph::edges(&pool, repo).await.unwrap();
    assert!(nodes
        .iter()
        .all(|node| node.key.source_path != "pkg/service.py"));
    assert!(edges.iter().any(|edge| edge.relation == CodeRelation::Calls
        && nodes
            .iter()
            .any(|node| node.id == edge.to && node.key.kind == CodeNodeKind::ExternalDependency)));
}

#[tokio::test]
async fn importer_pages_capture_missing_and_deleted_targets_without_losing_remainder() {
    let caller = "from .service import run\ndef main():\n    run()\n";
    let (_dir, pool, repo, _, _) = fixture(&[
        ("pkg/a.py", caller),
        ("pkg/b.py", caller),
        ("pkg/service.py", "# run is not defined yet\n"),
    ])
    .await;
    let changed = vec!["pkg/service.py".to_owned()];
    let first = codegraph::affected_importers_page(&pool, repo, &changed, None, 1)
        .await
        .unwrap();
    assert_eq!(first.paths, ["pkg/a.py"]);
    assert!(first.truncated);
    let second = codegraph::affected_importers_page(
        &pool,
        repo,
        &changed,
        first.paths.last().map(String::as_str),
        1,
    )
    .await
    .unwrap();
    assert_eq!(second.paths, ["pkg/b.py"]);
    assert!(!second.truncated);
    assert!(
        codegraph::affected_importers(&pool, RepositoryId::new(), &changed, 10)
            .await
            .unwrap()
            .paths
            .is_empty()
    );

    codegraph::upsert_file_graph(
        &pool,
        repo,
        &GitRevision("defined".into()),
        "pkg/service.py",
        "def run():\n    return 42\n",
    )
    .await
    .unwrap();
    for path in first.paths.iter().chain(&second.paths) {
        codegraph::reconcile_imports_for_file(
            &pool,
            repo,
            &GitRevision("defined".into()),
            path,
            caller,
        )
        .await
        .unwrap();
    }
    let captured = codegraph::affected_importers(&pool, repo, &changed, 10)
        .await
        .unwrap();
    assert_eq!(captured.paths, ["pkg/a.py", "pkg/b.py"]);
    let nodes = codegraph::nodes(&pool, repo).await.unwrap();
    let edges = codegraph::edges(&pool, repo).await.unwrap();
    for path in &captured.paths {
        assert!(linked(
            &nodes,
            &edges,
            (path, "main"),
            CodeRelation::Calls,
            ("pkg/service.py", "run")
        ));
    }
    // A definition signature change retires its prior symbol identity. Reparse
    // unchanged importers to restore calls to the replacement definition.
    codegraph::upsert_file_graph(
        &pool,
        repo,
        &GitRevision("signature".into()),
        "pkg/service.py",
        "def run(value: int):\n    return value\n",
    )
    .await
    .unwrap();
    for path in &captured.paths {
        codegraph::reconcile_imports_for_file(
            &pool,
            repo,
            &GitRevision("signature".into()),
            path,
            caller,
        )
        .await
        .unwrap();
    }
    let captured = codegraph::affected_importers(&pool, repo, &changed, 10)
        .await
        .unwrap();
    assert_eq!(captured.paths, ["pkg/a.py", "pkg/b.py"]);
    codegraph::remove_file_graph(&pool, repo, "pkg/service.py")
        .await
        .unwrap();
    for path in &captured.paths {
        codegraph::reconcile_imports_for_file(
            &pool,
            repo,
            &GitRevision("deleted".into()),
            path,
            caller,
        )
        .await
        .unwrap();
    }
    let nodes = codegraph::nodes(&pool, repo).await.unwrap();
    assert!(nodes
        .iter()
        .all(|node| node.key.source_path != "pkg/service.py"));
    assert_eq!(
        codegraph::affected_importers(&pool, repo, &changed, 10)
            .await
            .unwrap()
            .paths,
        ["pkg/a.py", "pkg/b.py"]
    );
}
