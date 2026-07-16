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

pub mod db;
pub mod outbox;
pub mod types;

pub use types::{
    CapabilityRequest, CodeEdge, CodeNode, CodeNodeKind, CodeRelation, ContentHash, EvidenceKind,
    EvidenceRef, GitRevision, JsonSchema, LanguageId, MemoryClass, MemoryRecord, Provenance,
    RegistryDependency, RegistryItem, RegistryItemKind, RegistryStatus, RetentionPolicy, Revision,
    RiskClass, Scope, SymbolKey, ToolCard, TrustMetadata, TrustTier, UsageExample, Version,
};

pub use outbox::KnowledgeIndexEvent;
