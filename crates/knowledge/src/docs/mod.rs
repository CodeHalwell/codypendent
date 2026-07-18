//! The collaborative Docs Studio (Phase 4, Chapter 08).
//!
//! Three layers:
//! - [`model`] — the stable, block-structured document domain types (the lossless
//!   export/import form);
//! - [`crdt`] — the Loro-backed live document (ADR-016), authoritative for a
//!   draft, that always projects back into [`model`] types;
//! - [`store`] — SQLite persistence of the CRDT snapshot plus a per-mutation
//!   authorship log, following the fabric's outbox conventions.
//!
//! Collaboration modes and suggestions (STEP 4.3), deterministic Markdown
//! rendering and Git publication (STEP 4.4), and the symbol-link staleness engine
//! (STEP 4.6) are layered on in their own modules.

pub mod apply;
pub mod collab;
pub mod crdt;
pub mod model;
pub mod render;
pub mod staleness;
pub mod store;

pub use apply::{apply_mutation, ApplyError, MutationEffect, MutationOutcome};
pub use crdt::{DocCrdtError, DocumentCrdt};
pub use model::{
    AuthorshipRecord, BlockContent, ChecklistItem, Citation, DocumentAuthor, DocumentBlock,
    DocumentLink, DocumentMetadata, DocumentRelation, DocumentStatus, KnowledgeDocument,
    LinkTarget, MutationKind, ResolvedSymbol,
};
pub use store::{DocStoreError, Document, DocumentStore, DocumentSummary, NewDocument};
