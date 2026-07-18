//! The knowledge fabric's domain contracts (Chapters 05–07).
//!
//! These are the shared shapes the registry, retrieval, memory, and code-graph
//! modules all speak. Enums that land in *scalar* SQL columns (so retrieval can
//! filter on them) carry an [`as_str`]/[`FromStr`] pair; everything richer is a
//! JSON column via `serde`.

use chrono::{DateTime, Utc};
use codypendent_protocol::{
    ArtifactRef, BranchId, DataClassification, OrganizationId, RegistryItemId, RepositoryId,
    SessionId, TaskId, UserId, WorkspaceId,
};
use serde::{Deserialize, Serialize};

/// A JSON Schema is carried opaquely; the registry never interprets it, it only
/// discloses it to the model after a tool is selected (progressive disclosure).
pub type JsonSchema = serde_json::Value;

// --------------------------------------------------------------------------
// Scope (Chapter 06)
// --------------------------------------------------------------------------

/// Where a registry item or memory lives and is visible. Cross-repository
/// inference is forbidden: a `Repository` scope never matches another repo, and
/// retrieval enforces this with a SQL filter on the flattened
/// [`tier`](Scope::tier)/[`key`](Scope::key), never by heuristic.
// Adjacently tagged (`tier` + `key`) so the id-bearing variants round-trip:
// internal tagging (`tag` only) cannot serialize a newtype variant wrapping a
// UUID (which serializes as a bare string), whereas adjacent tagging emits
// `{"tier":"repository","key":"<uuid>"}` and `{"tier":"system"}` — exactly the
// flattened shape stored in `scope_tier`/`scope_key`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tier", content = "key", rename_all = "snake_case")]
pub enum Scope {
    System,
    Organization(OrganizationId),
    User(UserId),
    Workspace(WorkspaceId),
    Repository(RepositoryId),
    Branch(BranchId),
    Session(SessionId),
    Task(TaskId),
}

impl Scope {
    /// The scalar tier string stored in `scope_tier` (indexed, SQL-filterable).
    #[must_use]
    pub fn tier(&self) -> &'static str {
        match self {
            Scope::System => "system",
            Scope::Organization(_) => "organization",
            Scope::User(_) => "user",
            Scope::Workspace(_) => "workspace",
            Scope::Repository(_) => "repository",
            Scope::Branch(_) => "branch",
            Scope::Session(_) => "session",
            Scope::Task(_) => "task",
        }
    }

    /// The scope's entity id as a string, stored in `scope_key` — `None` for the
    /// keyless `System` tier. This is the value cross-scope isolation compares.
    #[must_use]
    pub fn key(&self) -> Option<String> {
        match self {
            Scope::System => None,
            Scope::Organization(id) => Some(id.to_string()),
            Scope::User(id) => Some(id.to_string()),
            Scope::Workspace(id) => Some(id.to_string()),
            Scope::Repository(id) => Some(id.to_string()),
            Scope::Branch(id) => Some(id.to_string()),
            Scope::Session(id) => Some(id.to_string()),
            Scope::Task(id) => Some(id.to_string()),
        }
    }

    /// Precedence for shadowing: a more specific scope overrides a broader one of
    /// the same identity (workspace skill shadows user skill in *selection*,
    /// though both remain visible). Higher wins.
    #[must_use]
    pub fn specificity(&self) -> u8 {
        match self {
            Scope::System => 0,
            Scope::Organization(_) => 1,
            Scope::User(_) => 2,
            Scope::Workspace(_) => 3,
            Scope::Repository(_) => 4,
            Scope::Branch(_) => 5,
            Scope::Session(_) => 6,
            Scope::Task(_) => 7,
        }
    }
}

// --------------------------------------------------------------------------
// Registry item (Chapter 05)
// --------------------------------------------------------------------------

/// The kind of thing the registry governs. Stored scalar in `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryItemKind {
    Tool,
    Skill,
    Plugin,
    Hook,
    Command,
}

/// Coarse risk class, used both as a ranking penalty and (for the high end) a
/// disclosure signal. Ordered; stored scalar in `risk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    Safe,
    Low,
    Medium,
    High,
}

impl RiskClass {
    /// Derive a coarse risk from the requested capabilities: any write, command,
    /// or network request is at least `Medium`; secrets push to `High`.
    #[must_use]
    pub fn from_permissions(permissions: &[CapabilityRequest]) -> Self {
        let mut risk = RiskClass::Safe;
        for permission in permissions {
            let this = match permission {
                CapabilityRequest::FilesystemRead(_) => RiskClass::Low,
                CapabilityRequest::FilesystemWrite(_)
                | CapabilityRequest::Command(_)
                | CapabilityRequest::Network(_) => RiskClass::Medium,
                CapabilityRequest::Secret(_) => RiskClass::High,
            };
            risk = risk.max(this);
        }
        risk
    }
}

/// One requested capability, flattened from a skill's `[permissions]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CapabilityRequest {
    FilesystemRead(String),
    FilesystemWrite(String),
    Command(String),
    Network(String),
    Secret(String),
}

/// How trusted an item's *provenance* is — never inferred from relevance.
/// Stored scalar in `trust_tier`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustTier {
    Untrusted,
    Community,
    Verified,
    FirstParty,
}

/// Trust facts recorded for every item ([`skill.toml`] `[trust]` plus derived).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustMetadata {
    pub publisher: String,
    pub signature_required: bool,
    pub signature: Option<String>,
    pub tier: TrustTier,
}

/// Where an item came from — a built-in tool, or a package on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case")]
pub enum Provenance {
    /// A Phase-1 built-in tool, now registered with metadata.
    BuiltIn,
    /// A skill package on disk, at the recorded directory.
    Package { path: String },
}

/// A worked example of when to use an item (helps dense/lexical relevance).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageExample {
    pub query: String,
    pub note: Option<String>,
}

/// A dependency on another registry item (a skill pulls its required tools).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryDependency {
    /// The dependency target, by registry id string (a tool/skill name).
    pub target: String,
    pub optional: bool,
}

/// The lifecycle status of a registry item. Stored scalar in `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryStatus {
    Draft,
    Active,
    /// A package file changed without a version bump — flagged for the UI.
    Modified,
    Deprecated,
}

/// A semantic version string (e.g. `"0.1.0"`), validated as three dotted parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Version(pub String);

impl Version {
    /// Whether the string is a plain `MAJOR.MINOR.PATCH` (Phase 2 keeps semver
    /// minimal — no pre-release/build metadata parsing yet).
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let parts: Vec<&str> = self.0.split('.').collect();
        parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    }
}

/// A governed registry entry (Chapter 05 `RegistryItem`), plus the Phase-2
/// lifecycle fields (`status`, `content_hash`, `executable`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegistryItem {
    pub id: RegistryItemId,
    pub kind: RegistryItemKind,
    pub name: String,
    pub version: Version,
    pub scope: Scope,
    pub description: String,
    pub intents: Vec<String>,
    pub keywords: Vec<String>,
    pub examples: Vec<UsageExample>,
    pub input_schema: Option<JsonSchema>,
    pub output_schema: Option<JsonSchema>,
    pub dependencies: Vec<RegistryDependency>,
    pub permissions: Vec<CapabilityRequest>,
    pub risk: RiskClass,
    pub provenance: Provenance,
    pub trust: TrustMetadata,
    pub status: RegistryStatus,
    /// Hash over the item's package files — a change without a version bump
    /// flips `status` to `Modified`.
    pub content_hash: String,
    /// `false` when the item depends on scripts, which are not runnable until the
    /// Phase-6 sandbox — retrieval must not select a script-dependent behaviour.
    pub executable: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Progressive-disclosure card (Chapter 05). Compact cards go into context;
/// full JSON schemas load only for items the model actually selects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCard {
    pub id: RegistryItemId,
    pub kind: RegistryItemKind,
    pub name: String,
    pub summary: String,
    pub risk: RiskClass,
}

impl ToolCard {
    /// Byte ceiling for a card summary. A card is *progressive disclosure* — a
    /// registry item's description is authored text (possibly community-sourced),
    /// so an unbounded copy would let one item flood the context budget.
    pub const MAX_SUMMARY_BYTES: usize = 280;

    /// The compact card for an item (its description, truncated to
    /// [`Self::MAX_SUMMARY_BYTES`] on a char boundary).
    #[must_use]
    pub fn of(item: &RegistryItem) -> Self {
        let mut summary = item.description.clone();
        if summary.len() > Self::MAX_SUMMARY_BYTES {
            let mut end = Self::MAX_SUMMARY_BYTES;
            while end > 0 && !summary.is_char_boundary(end) {
                end -= 1;
            }
            summary.truncate(end);
            summary.push('…');
        }
        Self {
            id: item.id,
            kind: item.kind,
            name: item.name.clone(),
            summary,
            risk: item.risk,
        }
    }
}

// --------------------------------------------------------------------------
// Memory (Chapter 06)
// --------------------------------------------------------------------------

/// The class of a memory (Chapter 06 table). Stored scalar in `class`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryClass {
    Working,
    Episodic,
    Semantic,
    Procedural,
    Preference,
    Failure,
    Artifact,
    Code,
}

/// A git-or-logical revision string a memory is valid from/until.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Revision(pub String);

/// How long a memory is retained; the default is 365 days (Chapter 06).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// `None` means retain indefinitely; `Some(days)` expires after that.
    pub ttl_days: Option<u32>,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            ttl_days: Some(365),
        }
    }
}

/// A pointer from a memory (or edge) back to the evidence that produced it —
/// an event range or an artifact — so the client can always open its source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvidenceRef {
    /// A `[from_sequence, to_sequence]` span of one session's event ledger.
    EventRange {
        session_id: SessionId,
        from_sequence: u64,
        to_sequence: u64,
    },
    /// A stored artifact (optionally a file path it was captured from).
    Artifact {
        artifact: ArtifactRef,
        source_path: Option<String>,
    },
}

/// A curated memory (Chapter 06 `MemoryRecord`). A newer contradicting fact
/// supersedes — never deletes — an older one via `supersedes` + `valid_until`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: codypendent_protocol::MemoryId,
    pub class: MemoryClass,
    pub scope: Scope,
    pub statement: String,
    pub structured_value: Option<serde_json::Value>,
    /// At least one evidence ref is required — evidence-free candidates are
    /// rejected by the curator.
    pub provenance: Vec<EvidenceRef>,
    pub confidence: f32,
    pub observed_at: DateTime<Utc>,
    pub valid_from: Revision,
    pub valid_until: Option<Revision>,
    pub supersedes: Vec<codypendent_protocol::MemoryId>,
    pub sensitivity: DataClassification,
    pub retention: RetentionPolicy,
}

// --------------------------------------------------------------------------
// Code graph (Chapter 07)
// --------------------------------------------------------------------------

/// A language identifier, e.g. `"rust"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LanguageId(pub String);

/// A content hash (hex SHA-256), used for signatures and dedup cache keys.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(pub String);

/// A git revision (commit-ish) the graph snapshot belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GitRevision(pub String);

/// The kind of a code-graph node (Chapter 07 `CodeNodeKind`). Stored scalar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeNodeKind {
    Repository,
    Package,
    Module,
    File,
    Namespace,
    Type,
    TraitOrInterface,
    Function,
    Method,
    Field,
    Global,
    Constant,
    Endpoint,
    DatabaseTable,
    Test,
    Configuration,
    ExternalDependency,
}

/// The kind of a code-graph edge (Chapter 07 `CodeRelation`). Stored scalar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeRelation {
    Contains,
    Defines,
    Imports,
    References,
    Calls,
    Implements,
    Extends,
    Reads,
    Writes,
    Mutates,
    Returns,
    Accepts,
    Tests,
    Configures,
    Serializes,
    DependsOn,
    GeneratedFrom,
}

/// Which evidence layer produced an edge, and thus its confidence tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// Tree-sitter, as-written (Phase 2). Confidence ~0.45 for calls.
    SyntaxInferred,
    /// LSP / SCIP resolved (Phase 4). ~0.90.
    LspResolved,
    /// Compiler/indexer resolution. ~0.98.
    CompilerResolved,
    /// Observed at runtime. 1.00 for that execution.
    RuntimeObserved,
}

/// Confidence for a syntax-inferred call edge (Chapter 07 table).
pub const SYNTAX_CALL_CONFIDENCE: f32 = 0.45;

/// Confidence for an LSP-resolved reference/definition edge (Chapter 07 table).
/// A semantic edge at this confidence supersedes its syntax-inferred counterpart.
pub const LSP_RESOLVED_CONFIDENCE: f32 = 0.90;

/// Confidence for a compiler/indexer-resolved edge (Chapter 07 table).
pub const COMPILER_RESOLVED_CONFIDENCE: f32 = 0.98;

/// The stable identity of a symbol (Chapter 07 `SymbolKey`) — survives line
/// movement within its file because it is derived from name + kind + signature,
/// not byte position. `source_path` scopes that identity to the file the symbol
/// is defined in, so a same-named, same-signature symbol in a different file is
/// a distinct node (without it, two files' top-level `fn init`s would collapse
/// onto one row; see issue #6 item 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolKey {
    pub repository: RepositoryId,
    pub language: LanguageId,
    pub package: Option<String>,
    /// The repo-relative file the symbol was parsed from. Part of the identity:
    /// two files never share a symbol node, and a single-file reparse can retire
    /// exactly the symbols that file no longer defines.
    pub source_path: String,
    pub qualified_name: String,
    pub kind: CodeNodeKind,
    pub signature_hash: Option<ContentHash>,
}

impl SymbolKey {
    /// A stable composite string (`source_path|package::qualified_name#kind@sig`)
    /// used as the durable per-repository identity in `code_nodes.symbol_key`.
    /// Independent of byte position, so moving a symbol *within its file* does not
    /// change it; scoped by `source_path`, so the same name+signature in another
    /// file is a separate identity.
    #[must_use]
    pub fn stable_key(&self) -> String {
        let package = self.package.as_deref().unwrap_or("");
        let signature = self
            .signature_hash
            .as_ref()
            .map(|h| h.0.as_str())
            .unwrap_or("");
        format!(
            "{}|{package}::{}#{:?}@{signature}",
            self.source_path, self.qualified_name, self.kind
        )
    }
}

/// A node in the code graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeNode {
    pub id: codypendent_protocol::CodeNodeId,
    pub key: SymbolKey,
    pub revision: GitRevision,
}

/// An evidence-backed edge in the code graph (Chapter 07 `CodeEdge`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeEdge {
    pub from: codypendent_protocol::CodeNodeId,
    pub to: codypendent_protocol::CodeNodeId,
    pub relation: CodeRelation,
    pub confidence: f32,
    pub evidence_kind: EvidenceKind,
    pub evidence: Option<EvidenceRef>,
    pub revision: GitRevision,
}
