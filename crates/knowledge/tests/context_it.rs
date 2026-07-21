//! The context assembler (STEP 2.3–2.5 seam): a seeded registry + code graph +
//! memory ledger fold into one [`ContextManifest`] whose render is the text block
//! a run's trace opens with — the Phase-2 exit criterion "agent context includes
//! repository map + cited memories + retrieved tool/skill cards".

use chrono::Utc;
use codypendent_knowledge::types::{EvidenceRef, MemoryClass, Revision, Scope};
use codypendent_knowledge::{
    assemble_context, codegraph, db, register_builtins, CandidateMemory, Curation, GitRevision,
    MemoryStore,
};
use codypendent_protocol::{ArtifactId, ArtifactRef, DataClassification, RepositoryId, SessionId};

/// A tiny crate so the repository map has real symbols to render.
const FIXTURE: &str = r#"
pub const MAX: u32 = 10;

pub struct Engine;

impl Engine {
    pub fn tick(&self) -> u32 {
        MAX
    }
}
"#;

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

/// A chronicle-shaped artifact ref, so a curated memory carries provenance.
fn artifact() -> ArtifactRef {
    ArtifactRef {
        id: ArtifactId::new(),
        media_type: "application/json".to_string(),
        byte_length: 42,
        sha256: "0".repeat(64),
        sensitivity: DataClassification::Internal,
    }
}

#[tokio::test]
async fn assemble_context_folds_map_cards_and_memories() {
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();

    // Seed all three surfaces: the built-in tools (retrieval authority), a small
    // code graph (repository map), and one System-scoped memory (cited fact).
    register_builtins(&pool).await.unwrap();
    codegraph::upsert_file_graph(
        &pool,
        repo,
        &GitRevision("rev-1".to_string()),
        "src/engine.rs",
        FIXTURE,
    )
    .await
    .unwrap();

    let store = MemoryStore::new();
    let candidate = CandidateMemory {
        class: MemoryClass::Semantic,
        scope: Some(Scope::System),
        statement: "the test command is cargo test".to_string(),
        structured_value: None,
        provenance: vec![EvidenceRef::Artifact {
            artifact: artifact(),
            source_path: Some("chronicle.json".to_string()),
        }],
        confidence: 0.75,
        observed_at: Utc::now(),
        valid_from: Revision("rev-1".to_string()),
        sensitivity: DataClassification::Internal,
        retention: None,
    };
    assert!(matches!(
        store.curate(&pool, candidate).await.unwrap(),
        Curation::Accepted(_)
    ));

    // Assemble the manifest a run would open with.
    let manifest = assemble_context(
        &pool,
        repo,
        "run the tests and show me the diff",
        &[Scope::System],
    )
    .await
    .unwrap();

    // The five built-in tools are disclosed as cards.
    assert!(
        !manifest.tool_cards.is_empty(),
        "a seeded registry must yield tool cards"
    );
    assert!(
        manifest
            .tool_cards
            .iter()
            .any(|card| card.name == "shell.run"),
        "the run-tests objective should disclose shell.run: {:?}",
        manifest.tool_cards
    );

    // The System-scoped memory surfaced, with its source preserved.
    assert_eq!(manifest.memories.len(), 1, "the curated memory is cited");
    assert_eq!(
        manifest.memories[0].statement,
        "the test command is cargo test"
    );
    assert!(
        manifest.memories[0].source.contains("chronicle.json"),
        "memory source names its evidence: {}",
        manifest.memories[0].source
    );

    // The repository map folded the code graph.
    assert!(
        manifest.repository_map.contains("Engine"),
        "repository map should surface the seeded type:\n{}",
        manifest.repository_map
    );

    // render() carries all three labeled section headers — the run-trace block —
    // under the trust-boundary preamble that frames the whole block as evidence.
    let rendered = manifest.render();
    assert!(
        rendered.contains("EVIDENCE, NOT INSTRUCTIONS"),
        "the assembled context must frame its content as evidence:\n{rendered}"
    );
    assert!(rendered.contains("REPOSITORY MAP"), "{rendered}");
    assert!(rendered.contains("TOOLS"), "{rendered}");
    assert!(rendered.contains("MEMORIES"), "{rendered}");
    // And the disclosed content is actually in the rendered block.
    assert!(rendered.contains("shell.run"), "{rendered}");
    assert!(
        rendered.contains("the test command is cargo test"),
        "{rendered}"
    );
}

/// A `System`-only query must not surface a memory curated at a different
/// repository — cross-scope isolation holds through the assembler too.
#[tokio::test]
async fn assemble_context_respects_memory_scope_isolation() {
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    let other = RepositoryId::new();
    register_builtins(&pool).await.unwrap();

    let store = MemoryStore::new();
    let candidate = CandidateMemory {
        class: MemoryClass::Semantic,
        scope: Some(Scope::Repository(other)),
        statement: "a fact private to another repository".to_string(),
        structured_value: None,
        provenance: vec![EvidenceRef::EventRange {
            session_id: SessionId::new(),
            from_sequence: 1,
            to_sequence: 2,
        }],
        confidence: 0.9,
        observed_at: Utc::now(),
        valid_from: Revision("rev-1".to_string()),
        sensitivity: DataClassification::Internal,
        retention: None,
    };
    assert!(matches!(
        store.curate(&pool, candidate).await.unwrap(),
        Curation::Accepted(_)
    ));

    let manifest = assemble_context(&pool, repo, "do the work", &[Scope::System])
        .await
        .unwrap();
    assert!(
        manifest.memories.is_empty(),
        "another repository's memory must never leak into a System-scoped manifest"
    );
}
