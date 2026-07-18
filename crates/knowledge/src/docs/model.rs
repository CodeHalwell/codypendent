//! The collaborative document domain model (Chapter 08, STEP 4.2).
//!
//! A [`KnowledgeDocument`] is an ordered list of typed [`DocumentBlock`]s with
//! knowledge-graph [`DocumentLink`]s, [`Citation`]s, and a per-mutation
//! [`AuthorshipRecord`] log. The block-structured model here is the **stable,
//! losslessly round-trippable** representation (serde JSON); the live CRDT
//! (`super::crdt`) is authoritative for a *draft* (ADR-004/016) but always
//! projects back into these types, so `blocks → CRDT → blocks` is an identity.

use chrono::{DateTime, Utc};
use codypendent_protocol::{DocumentId, ModelId, RunId, UserId};
use serde::{Deserialize, Serialize};

use crate::types::{CodeNodeKind, EvidenceRef, Scope};

/// The lifecycle status of a document. Stored scalar in `documents.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentStatus {
    /// Actively edited; the CRDT is authoritative.
    Draft,
    /// Frozen for review before publication.
    InReview,
    /// A reviewed snapshot has been published to Git (STEP 4.4).
    Published,
    /// Retained but no longer maintained.
    Archived,
}

impl DocumentStatus {
    /// The scalar `status` column string.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            DocumentStatus::Draft => "draft",
            DocumentStatus::InReview => "in_review",
            DocumentStatus::Published => "published",
            DocumentStatus::Archived => "archived",
        }
    }
}

/// Descriptive metadata carried alongside a document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentMetadata {
    /// A one-line summary (indexed for retrieval).
    pub summary: Option<String>,
    /// Free-form tags.
    pub tags: Vec<String>,
    /// The owning person or team (a `OWNED_BY` link is the graph form).
    pub owner: Option<String>,
    /// Who created the document.
    pub created_by: Option<DocumentAuthor>,
}

/// Who performed a mutation (Chapter 08 `DocumentAuthor`). An agent sentence is
/// always traceable to its run, model, and the policy version in force.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DocumentAuthor {
    /// A human editor.
    Human { user: UserId },
    /// An agent run, with the traceability triple.
    Agent {
        run_id: RunId,
        model: ModelId,
        policy_version: String,
    },
    /// An external integration (e.g. a Notion sync).
    Integration { integration: String },
}

/// The kind of mutation an [`AuthorshipRecord`] attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationKind {
    /// A new block was inserted.
    InsertBlock,
    /// A block was removed.
    DeleteBlock,
    /// Text was inserted/deleted inside a text block (a CRDT text op).
    EditText,
    /// A block's content was replaced wholesale (structured blocks).
    SetBlock,
    /// A suggestion (proposed range + replacement) was recorded.
    Suggest,
    /// A suggestion was accepted and applied.
    AcceptSuggestion,
    /// A suggestion was rejected without applying.
    RejectSuggestion,
}

impl MutationKind {
    /// The scalar `mutation` column string.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            MutationKind::InsertBlock => "insert_block",
            MutationKind::DeleteBlock => "delete_block",
            MutationKind::EditText => "edit_text",
            MutationKind::SetBlock => "set_block",
            MutationKind::Suggest => "suggest",
            MutationKind::AcceptSuggestion => "accept_suggestion",
            MutationKind::RejectSuggestion => "reject_suggestion",
        }
    }
}

/// One entry in a document's attribution log — a single mutation and who made it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorshipRecord {
    pub author: DocumentAuthor,
    /// The block the mutation targeted, when it targets one.
    pub block_id: Option<String>,
    pub mutation: MutationKind,
    /// The document revision this mutation produced.
    pub revision: u64,
    pub at: DateTime<Utc>,
}

/// A typed block of document content (Chapter 08 block kinds). Internally tagged
/// so the canonical JSON export reads `{"type":"heading","level":1,"text":"…"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockContent {
    Heading {
        level: u8,
        text: String,
    },
    Paragraph {
        text: String,
    },
    Code {
        language: Option<String>,
        text: String,
    },
    Diagram {
        format: String,
        source: String,
    },
    Table {
        rows: Vec<Vec<String>>,
    },
    Callout {
        kind: String,
        text: String,
    },
    Checklist {
        items: Vec<ChecklistItem>,
    },
    /// A live query block (Chapter 08 `Query`) — the query string is stored; its
    /// evaluation is a later concern.
    Query {
        query: String,
    },
    EmbeddedFile {
        path: String,
    },
    /// A `{{ symbol:path::to::symbol }}` reference resolved against the code graph
    /// (STEP 4.6). `symbol` is the qualified name as written.
    EmbeddedSymbol {
        symbol: String,
    },
    EmbeddedWorkflow {
        workflow: String,
    },
    EmbeddedSkill {
        skill: String,
    },
}

/// One item in a [`BlockContent::Checklist`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecklistItem {
    pub text: String,
    pub checked: bool,
}

/// A document block: a stable id plus its typed content. The id survives merges
/// and reorders (it is the CRDT map's `id` field), so links and citations can
/// anchor to a block by id rather than by position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentBlock {
    pub id: String,
    #[serde(flatten)]
    pub content: BlockContent,
}

impl DocumentBlock {
    /// A new block with a fresh time-ordered id.
    #[must_use]
    pub fn new(content: BlockContent) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            content,
        }
    }

    /// A block with a caller-chosen id (used when rebuilding from storage/CRDT so
    /// ids are preserved).
    #[must_use]
    pub fn with_id(id: impl Into<String>, content: BlockContent) -> Self {
        Self {
            id: id.into(),
            content,
        }
    }

    /// The primary editable text of this block, or `""` for structured/embed
    /// blocks. A convenience over [`Self::primary_text`].
    #[must_use]
    pub fn content_text(&self) -> &str {
        self.primary_text().unwrap_or("")
    }

    /// The primary editable text of this block, if it has one (the value stored
    /// in the CRDT's `text` container). `None` for purely structured/embed blocks.
    #[must_use]
    pub fn primary_text(&self) -> Option<&str> {
        match &self.content {
            BlockContent::Heading { text, .. }
            | BlockContent::Paragraph { text }
            | BlockContent::Code { text, .. }
            | BlockContent::Callout { text, .. }
            | BlockContent::Query { query: text } => Some(text),
            BlockContent::Diagram { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A knowledge-graph relation from a document to another entity (Chapter 08).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentRelation {
    References,
    OwnedBy,
    Implements,
    UsedBy,
    Supersedes,
}

/// The target of a [`DocumentLink`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum LinkTarget {
    /// A code symbol by qualified name (e.g. `payments::charge_customer`).
    Symbol(String),
    Workflow(String),
    Skill(String),
    Team(String),
    Document(DocumentId),
}

/// A resolved code-symbol identity recorded on a `Symbol` link when the publisher
/// resolves it against the code graph (STEP 4.6). Staleness compares a later
/// graph revision's symbol against this snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSymbol {
    /// `SymbolKey::stable_key()` of the resolved node.
    pub symbol_key: String,
    /// The repo-relative file the symbol was defined in at resolution time. Part
    /// of the identity staleness matches on, so two symbols that share a
    /// qualified name in different files are tracked independently (a change to
    /// the *other* file's symbol never flags this link).
    pub source_path: String,
    /// The node kind at resolution time (e.g. `Type` vs `TraitOrInterface`). Part
    /// of symbol identity: a `struct Foo` becoming a `trait Foo` is a stale-making
    /// change even when neither carries a signature hash (so the hash comparison
    /// alone — `None == None` — would miss it).
    pub kind: CodeNodeKind,
    /// The signature hash at resolution time — a change flags staleness.
    pub signature_hash: Option<String>,
    /// The graph revision the link was resolved at.
    pub revision: String,
}

/// A knowledge-graph link from a document (Chapter 08 links section).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentLink {
    pub relation: DocumentRelation,
    pub target: LinkTarget,
    /// The block this link originates from, when it is anchored to one (e.g. an
    /// `EmbeddedSymbol` block).
    pub block_id: Option<String>,
    /// The resolved symbol identity (populated for `Symbol` targets by the
    /// publisher/staleness engine; `None` until resolved).
    pub resolved: Option<ResolvedSymbol>,
}

/// A citation backing a claim in a document with evidence (Chapter 08). Reuses
/// the fabric's [`EvidenceRef`] so a citation always opens its source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    pub id: String,
    /// The block carrying the cited claim, when anchored.
    pub block_id: Option<String>,
    pub evidence: EvidenceRef,
    pub note: Option<String>,
}

/// A collaborative knowledge document (Chapter 08 `KnowledgeDocument`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeDocument {
    pub id: DocumentId,
    pub title: String,
    pub scope: Scope,
    pub status: DocumentStatus,
    pub metadata: DocumentMetadata,
    pub blocks: Vec<DocumentBlock>,
    pub links: Vec<DocumentLink>,
    pub citations: Vec<Citation>,
    pub authorship: Vec<AuthorshipRecord>,
    /// A monotonic per-document revision, bumped on each recorded mutation batch.
    /// The `(revision ↔ git commit)` pairing (STEP 4.4) lets staleness compare.
    pub revision: u64,
}
