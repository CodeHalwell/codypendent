//! Display projections for authenticated, daemon-owned knowledge browsing.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum KnowledgeSection {
    Skills,
    Memories,
    Learnings,
    Documents,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct KnowledgePage {
    pub section: KnowledgeSection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<crate::WorkspaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillCard>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<MemoryCard>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub learnings: Vec<LearningCard>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub documents: Vec<DocCard>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// `crates/tui/src/state.rs` `SkillCard`. `permissions` are verbatim strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct SkillCard {
    pub name: String,
    pub kind: String,
    pub scope: String,
    pub trust: String,
    pub status: String,
    pub risk: String,
    pub description: String,
    pub permissions: Vec<String>,
}

/// `crates/tui/src/state.rs` `MemoryCard`, plus the memory's id so the
/// desktop can address `CorrectMemory`/`ForgetMemory` at it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct MemoryCard {
    pub id: String,
    pub statement: String,
    pub class: String,
    pub scope: String,
    pub revision: String,
    /// A date string, exactly as the store rendered it.
    pub observed: String,
    pub confidence: f32,
    /// The human-readable evidence source; "(no evidence)" when there is none.
    pub source: String,
}

/// `crates/tui/src/state.rs` `LearningCard`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct LearningCard {
    pub id: String,
    pub statement: String,
    pub kind: String,
    pub state: String,
    pub scope: String,
    pub provenance: String,
    pub confidence: f32,
    pub pinned: bool,
    pub revision: u64,
}

/// `crates/tui/src/state.rs` `DocBlockView`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct DocBlockView {
    pub id: String,
    pub kind: String,
    /// A one-line, lossy display rendering.
    pub text: String,
    /// The block's primary text verbatim, or `None` for a structured block
    /// with no single editable container.
    pub editable: Option<String>,
}

/// `crates/tui/src/state.rs` `DocSuggestionView`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct DocSuggestionView {
    pub id: String,
    pub block_id: String,
    pub source_revision: u64,
    pub status: String,
    pub author: String,
    pub range: String,
    pub original: String,
    pub replacement: String,
    pub rationale: Option<String>,
}

/// `crates/tui/src/state.rs` `DocCard`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct DocCard {
    pub document_id: crate::DocumentId,
    pub title: String,
    pub scope: String,
    pub status: String,
    pub mode: String,
    /// Pre-rendered, e.g. `"r7"`.
    pub revision: String,
    pub blocks: Vec<DocBlockView>,
    pub suggestions: Vec<DocSuggestionView>,
}
