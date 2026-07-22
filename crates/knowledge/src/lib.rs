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
pub mod codegraph;
pub mod context;
pub mod db;
pub mod docs;
pub mod manifest;
pub mod memory;
pub mod observer;
pub mod outbox;
pub mod registry;
pub mod repomap;
pub mod retrieval;
pub mod skill_exec;
pub mod types;

pub use types::{
    CapabilityRequest, CodeEdge, CodeNode, CodeNodeKind, CodeRelation, ContentHash, EvidenceKind,
    EvidenceRef, GitRevision, JsonSchema, LanguageId, MemoryClass, MemoryRecord, Provenance,
    RegistryDependency, RegistryItem, RegistryItemKind, RegistryStatus, RetentionPolicy, Revision,
    RiskClass, Scope, SymbolKey, ToolCard, TrustMetadata, TrustTier, UsageExample, Version,
};

pub use outbox::KnowledgeIndexEvent;

pub use builtin::{builtin_tools, register_builtins};
pub use manifest::{
    load_package, ManifestError, SkillEntrypoints, SkillLimits, SkillManifest, SkillPermissions,
    SkillTrust,
};
pub use registry::{resolve_shadowed, Registry, RegistryError};
pub use skill_exec::{profile_for_permissions, run_script, SkillExecError};

pub use retrieval::{
    embedding_text, retrieve, Bm25Error, Bm25Index, Embedder, HashingEmbedder, RerankWeights,
    RetrievalConfig, RetrievalError, RetrievalIndexes, RetrievalQuery, RetrievalResult,
    RetrievalTrace, VectorIndex, EMBEDDING_DIMENSION,
};

pub use adapter::{
    BuildMetadata, Diagnostic, DiagnosticSeverity, LanguageAdapter, PackageInfo, ParseInput,
    ParseOutput, RustAdapter, ScriptAdapter, SemanticCapability, SymbolIndex, Workspace,
};
pub use codegraph::{
    changed_between, stable_repository_id, CodeGraphError, GraphDelta, ParsedSymbol, SemanticEdge,
    SymbolDelta, SymbolSnapshot,
};
pub use repomap::{
    hierarchical_map, ApiSymbol, MapEvidence, MapLevel, MapNode, ModuleEntry, PackageEntry,
    RepositoryMap,
};

pub use memory::{
    detect_secret, provenance_cards, CandidateMemory, Curation, ForgetAudit, MemoryError,
    MemoryStore, ProvenanceCard,
};
pub use observer::extract_candidates;

pub use context::{assemble_context, ContextCard, ContextError, ContextManifest, ContextMemory};

pub use docs::apply::{apply_mutation, ApplyError, MutationEffect, MutationOutcome};
pub use docs::collab::{
    CollaborationMode, EditDisposition, NewSuggestion, Suggestion, SuggestionStatus,
    SuggestionStore,
};
pub use docs::crdt::{DocCrdtError, DocumentCrdt};
pub use docs::leases::{DocumentLease, DocumentLeaseStore, LeaseError};
pub use docs::model::{
    AuthorshipRecord, BlockContent, ChecklistItem, Citation, DocumentAuthor, DocumentBlock,
    DocumentLink, DocumentMetadata, DocumentRelation, DocumentStatus, KnowledgeDocument,
    LinkTarget, MutationKind, ResolvedSymbol,
};
pub use docs::render::{
    plan_publication, publications, record_publication, render_document, Publication, PublishPlan,
    PublishTarget,
};
pub use docs::replica::DocumentReplica;
pub use docs::staleness::{
    detect_staleness, resolve_links, symbol_references, StalenessFinding, StalenessReason,
    SymbolRef,
};
pub use docs::store::{DocStoreError, Document, DocumentStore, DocumentSummary, NewDocument};
