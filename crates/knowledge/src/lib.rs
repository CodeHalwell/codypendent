//! codypendent-knowledge — the knowledge fabric (Phase 2).
//!
//! A governed **registry** of tools and skills, **hybrid retrieval** (dense +
//! BM25 + exact + history) with hard security filters, an always-on **memory**
//! fabric with provenance, and a syntax-layer **code graph**. It is a library
//! the daemon and runtime consume; it depends only on `codypendent-protocol`
//! (shared IDs + wire types) and never on the daemon or runtime — that
//! inversion keeps the fabric reusable and testable in isolation.
//!
//! Every authoritative write also appends an [`outbox`] row in the same
//! transaction; indexer workers replay the outbox into the derived indexes
//! (Tantivy, vectors) under `<data_dir>/index/`, which are deletable and
//! rebuildable at any time (`codypendent index rebuild`).

pub mod adapter;
pub mod builtin;
pub mod client_views;
pub mod codegraph;
pub mod context;
pub mod db;
pub mod docs;
pub mod extractor;
pub mod learning;
pub mod lsp;
pub mod manifest;
pub mod memory;
pub mod observer;
pub mod outbox;
pub mod poison;
pub mod registry;
pub mod repomap;
pub mod retrieval;
pub mod skill_exec;
pub mod skills;
pub mod types;

pub use types::{
    CapabilityRequest, CodeEdge, CodeNode, CodeNodeKind, CodeRelation, ContentHash, EvidenceKind,
    EvidenceRef, GitRevision, JsonSchema, LanguageId, MemoryClass, MemoryRecord, Provenance,
    RegistryDependency, RegistryItem, RegistryItemKind, RegistryStatus, RetentionPolicy, Revision,
    RiskClass, Scope, SymbolKey, ToolCard, TrustMetadata, TrustTier, UsageExample, Version,
    AGENT_ASSERTED_CONFIDENCE, COMPILER_RESOLVED_CONFIDENCE, LSP_RESOLVED_CONFIDENCE,
    RUNTIME_OBSERVED_CONFIDENCE, SYNTAX_CALL_CONFIDENCE,
};

pub use outbox::KnowledgeIndexEvent;

pub use extractor::{ExtractionInput, FactExtractor, NoopExtractor};

pub use builtin::{builtin_tools, register_builtins};
pub use learning::{
    ActivationIntent, ActivationOutcome, CaptureOutcome, DeletedLearning, LearningContent,
    LearningError, LearningKind, LearningPatch, LearningProcedure, LearningProvenance,
    LearningQuery, LearningRecord, LearningScope, LearningState, LearningStore,
    MutationOutcome as LearningMutationOutcome, NewLearning, Verification,
};
pub use manifest::{
    hash_package, load_package, ManifestError, SkillEntrypoints, SkillLimits, SkillManifest,
    SkillPermissions, SkillResourceLimits, SkillTrust,
};
pub use registry::{resolve_shadowed, Registry, RegistryError};
pub use skill_exec::{
    profile_for_permissions, run_module, run_script, substitute_placeholders, PlaceholderContext,
    SkillExecError, SkillInvocation, SkillRunOutcome, SkillRunner,
};
pub use skills::{
    anchor_repository_id, conventional_skill_roots, conventional_user_skill_roots, install_package,
    is_retrievable_status, local_user_scope, repository_skills_root, scan_skill_root,
    user_skills_root, SkillInstallError, SkillScanOutcome,
};

pub use retrieval::{
    drain_outbox, embedding_content_hash, embedding_text, reconcile_embeddings, retrieve,
    semantic_indexes, Bm25Error, Bm25Index, DrainReport, EmbedError, Embedder, HashingEmbedder,
    PersistError, RerankWeights, RetrievalConfig, RetrievalError, RetrievalIndexes, RetrievalQuery,
    RetrievalResult, RetrievalTrace, SemanticEmbedder, StoredEmbedding, VectorIndex,
    EMBEDDING_DIMENSION,
};

pub use adapter::{
    BuildMetadata, Diagnostic, DiagnosticSeverity, LanguageAdapter, PackageInfo, ParseInput,
    ParseOutput, RustAdapter, ScriptAdapter, SemanticCapability, SymbolIndex, Workspace,
};
pub use codegraph::{
    assert_agent_edges, changed_between, language_for, rebuild_repository, stable_repository_id,
    supported_extensions, AgentEdgeAssertion, AssertionResult, CarriedEdges, CodeGraphError,
    CodeGraphQueries, FoldedFile, GraphAnswer, GraphDelta, GraphHit, GraphQuestion, Language,
    ParsedSymbol, RepositoryRebuild, RetiredFiles, ScanCoverage, ScanSummary, SemanticEdge,
    SemanticUpsertOutcome, SymbolDelta, SymbolSnapshot, GRAPH_ANSWER_LIMIT, GRAPH_MAX_DEPTH,
};
pub use lsp::{report, LiveDiagnostics, LspDiagnostic, LspManager};
pub use repomap::{
    hierarchical_map, ApiSymbol, MapEvidence, MapLevel, MapNode, ModuleEntry, PackageEntry,
    RepositoryMap,
};

pub use memory::{
    detect_secret, provenance_cards, CandidateMemory, Curation, ForgetAudit, MemoryCorrection,
    MemoryError, MemoryStore, ProvenanceCard,
};
pub use observer::{chronicle_candidates, extract_candidates};

pub use context::{
    assemble_context, ContextAssembler, ContextCard, ContextError, ContextLearning,
    ContextManifest, ContextMemory,
};

pub use docs::apply::{apply_mutation, ApplyError, MutationEffect, MutationOutcome};
pub use docs::collab::{
    CollaborationMode, EditDisposition, NewSuggestion, Suggestion, SuggestionStatus,
    SuggestionStore,
};
pub use docs::crdt::{DocCrdtError, DocumentCrdt};
pub use docs::import::{import_markdown, markdown_to_blocks};
pub use docs::leases::{DocumentLease, DocumentLeaseStore, LeaseError};
pub use docs::model::{
    AuthorshipRecord, BlockContent, ChecklistItem, Citation, DocumentAuthor, DocumentBlock,
    DocumentLink, DocumentMetadata, DocumentRelation, DocumentStatus, KnowledgeDocument,
    LinkTarget, MutationKind, ResolvedSymbol,
};
pub use docs::render::{
    pending_pull_request_publications, plan_publication, publications, record_publication,
    record_pull_request_merge, render_document, Publication, PublishPlan, PublishTarget,
    PullRequestHandle,
};
pub use docs::replica::DocumentReplica;
pub use docs::staleness::{
    detect_staleness, resolve_links, symbol_references, StalenessFinding, StalenessReason,
    SymbolRef,
};
pub use docs::store::{DocStoreError, Document, DocumentStore, DocumentSummary, NewDocument};
