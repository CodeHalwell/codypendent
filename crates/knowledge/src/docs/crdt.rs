//! The Loro-backed CRDT layer for collaborative documents (STEP 4.2, ADR-016).
//!
//! A document maps into Loro as a root list `"blocks"` of map containers, one per
//! [`DocumentBlock`]. Each block map carries:
//! - `id` — the stable block id (a scalar string);
//! - `type` — the [`BlockContent`] discriminant (a scalar string);
//! - `text` — a Loro **text** container holding the block's primary editable text
//!   (character-level concurrent editing merges here);
//! - `meta` — a scalar JSON string holding every non-text attribute (heading
//!   level, code language, table rows, embed targets, …).
//!
//! Block insert/delete/reorder merge via the list CRDT; same-block text edits
//! merge via the text CRDT; disjoint edits from two replicas always converge
//! (exit criterion 1). The mapping is a bijection with [`DocumentBlock`]:
//! `blocks → CRDT → blocks` is the identity, so the block-structured export is
//! lossless even though Loro's internal representation differs.

use loro::{LoroDoc, LoroMap, LoroText, LoroValue, ValueOrContainer};
use serde_json::json;

use super::model::{BlockContent, ChecklistItem, DocumentBlock};

/// The root list container holding the document's blocks.
const BLOCKS: &str = "blocks";

/// Errors from the CRDT layer.
#[derive(Debug, thiserror::Error)]
pub enum DocCrdtError {
    /// A Loro operation failed.
    #[error("loro error: {0}")]
    Loro(String),
    /// The CRDT state did not have the expected block/field shape.
    #[error("document shape error: {0}")]
    Shape(String),
    /// A stored `meta` blob did not (de)serialize.
    #[error("meta serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// A referenced block id was not present.
    #[error("no such block: {0}")]
    NoSuchBlock(String),
    /// A text position/range fell outside the block's current text. Loro's text
    /// operations panic on out-of-bounds indices, and concurrent edits can shift
    /// or shorten a block between when a range is chosen and when it is applied
    /// (e.g. accepting a stale suggestion), so callers get a recoverable error
    /// instead of a daemon-crashing panic.
    #[error("text position {pos} out of bounds (block length {length})")]
    OutOfBounds { pos: usize, length: usize },
}

impl From<loro::LoroError> for DocCrdtError {
    fn from(e: loro::LoroError) -> Self {
        DocCrdtError::Loro(e.to_string())
    }
}

impl From<loro::LoroEncodeError> for DocCrdtError {
    fn from(e: loro::LoroEncodeError) -> Self {
        DocCrdtError::Loro(e.to_string())
    }
}

/// A live collaborative document backed by a Loro CRDT.
pub struct DocumentCrdt {
    doc: LoroDoc,
}

impl Default for DocumentCrdt {
    fn default() -> Self {
        Self::new()
    }
}

impl DocumentCrdt {
    /// An empty document.
    #[must_use]
    pub fn new() -> Self {
        Self {
            doc: LoroDoc::new(),
        }
    }

    /// Build a fresh document from a block list. Ids are preserved, so a
    /// round-trip through [`Self::to_blocks`] is an identity.
    pub fn from_blocks(blocks: &[DocumentBlock]) -> Result<Self, DocCrdtError> {
        let crdt = Self::new();
        for (i, block) in blocks.iter().enumerate() {
            crdt.insert_block(i, block)?;
        }
        crdt.doc.commit();
        Ok(crdt)
    }

    /// Load a document from a Loro snapshot (its authoritative draft state).
    pub fn from_snapshot(bytes: &[u8]) -> Result<Self, DocCrdtError> {
        let doc = LoroDoc::new();
        doc.import(bytes)?;
        Ok(Self { doc })
    }

    /// Export the authoritative snapshot (durable draft state).
    pub fn snapshot(&self) -> Result<Vec<u8>, DocCrdtError> {
        self.doc.commit();
        Ok(self.doc.export(loro::ExportMode::Snapshot)?)
    }

    /// Merge another replica's snapshot into this document. Convergent: applying
    /// two replicas' snapshots in either order yields identical content.
    pub fn merge_snapshot(&self, bytes: &[u8]) -> Result<(), DocCrdtError> {
        self.doc.import(bytes)?;
        Ok(())
    }

    /// The current block list, projected out of the CRDT.
    pub fn to_blocks(&self) -> Result<Vec<DocumentBlock>, DocCrdtError> {
        let list = self.doc.get_list(BLOCKS);
        let mut blocks = Vec::with_capacity(list.len());
        for i in 0..list.len() {
            let map = block_map(&list.get(i))?;
            blocks.push(read_block(&map)?);
        }
        Ok(blocks)
    }

    /// The number of blocks currently in the document.
    #[must_use]
    pub fn block_count(&self) -> usize {
        self.doc.get_list(BLOCKS).len()
    }

    /// Insert `block` at `index` (clamped to the current length).
    pub fn insert_block(&self, index: usize, block: &DocumentBlock) -> Result<(), DocCrdtError> {
        let list = self.doc.get_list(BLOCKS);
        let index = index.min(list.len());
        let map: LoroMap = list.insert_container(index, LoroMap::new())?;
        write_block(&map, block)?;
        self.doc.commit();
        Ok(())
    }

    /// Append `block` to the end of the document.
    pub fn push_block(&self, block: &DocumentBlock) -> Result<(), DocCrdtError> {
        self.insert_block(self.block_count(), block)
    }

    /// Delete the block with `block_id`. Errors if no such block exists.
    pub fn delete_block(&self, block_id: &str) -> Result<(), DocCrdtError> {
        let list = self.doc.get_list(BLOCKS);
        let index = self
            .index_of(block_id)?
            .ok_or_else(|| DocCrdtError::NoSuchBlock(block_id.to_string()))?;
        list.delete(index, 1)?;
        self.doc.commit();
        Ok(())
    }

    /// Replace the content of the block with `block.id` wholesale (used for
    /// structured blocks and programmatic sets). Text-block character history is
    /// not preserved by this op — prefer [`Self::insert_text`]/[`Self::delete_text`]
    /// for collaborative text editing.
    pub fn set_block(&self, block: &DocumentBlock) -> Result<(), DocCrdtError> {
        let map = self.block_map_by_id(&block.id)?;
        write_content(&map, block)?;
        self.doc.commit();
        Ok(())
    }

    /// Insert `text` at character position `pos` inside a text block — a real CRDT
    /// text op that merges with concurrent edits.
    pub fn insert_text(&self, block_id: &str, pos: usize, text: &str) -> Result<(), DocCrdtError> {
        let map = self.block_map_by_id(block_id)?;
        let container = text_container(&map)?;
        let length = container.len_unicode();
        if pos > length {
            return Err(DocCrdtError::OutOfBounds { pos, length });
        }
        container.insert(pos, text)?;
        self.doc.commit();
        Ok(())
    }

    /// Delete `len` characters at position `pos` inside a text block.
    pub fn delete_text(&self, block_id: &str, pos: usize, len: usize) -> Result<(), DocCrdtError> {
        let map = self.block_map_by_id(block_id)?;
        let container = text_container(&map)?;
        let length = container.len_unicode();
        // `pos + len` cannot overflow into bounds: check the endpoint against the
        // length without adding (which could wrap).
        if pos > length || len > length - pos {
            return Err(DocCrdtError::OutOfBounds { pos, length });
        }
        container.delete(pos, len)?;
        self.doc.commit();
        Ok(())
    }

    /// Replace a text block's whole text (clear + insert). Not merge-friendly;
    /// used for programmatic sets, not concurrent editing.
    pub fn replace_text(&self, block_id: &str, text: &str) -> Result<(), DocCrdtError> {
        let map = self.block_map_by_id(block_id)?;
        let container = text_container(&map)?;
        let len = container.len_unicode();
        if len > 0 {
            container.delete(0, len)?;
        }
        container.insert(0, text)?;
        self.doc.commit();
        Ok(())
    }

    /// The current index of the block with `block_id`, if present.
    fn index_of(&self, block_id: &str) -> Result<Option<usize>, DocCrdtError> {
        let list = self.doc.get_list(BLOCKS);
        for i in 0..list.len() {
            let map = block_map(&list.get(i))?;
            if string_field(&map, "id").as_deref() == Some(block_id) {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }

    /// The block map for `block_id`, or a [`DocCrdtError::NoSuchBlock`].
    fn block_map_by_id(&self, block_id: &str) -> Result<LoroMap, DocCrdtError> {
        let list = self.doc.get_list(BLOCKS);
        let index = self
            .index_of(block_id)?
            .ok_or_else(|| DocCrdtError::NoSuchBlock(block_id.to_string()))?;
        block_map(&list.get(index))
    }
}

// --------------------------------------------------------------------------
// Block <-> CRDT map mapping
// --------------------------------------------------------------------------

/// Coerce a list element into its block map container.
fn block_map(value: &Option<ValueOrContainer>) -> Result<LoroMap, DocCrdtError> {
    match value {
        Some(ValueOrContainer::Container(c)) => c
            .clone()
            .into_map()
            .map_err(|_| DocCrdtError::Shape("block is not a map container".into())),
        _ => Err(DocCrdtError::Shape(
            "block list element is not a container".into(),
        )),
    }
}

/// Read a scalar string field from a block map.
fn string_field(map: &LoroMap, key: &str) -> Option<String> {
    match map.get(key) {
        Some(ValueOrContainer::Value(LoroValue::String(s))) => Some(s.to_string()),
        _ => None,
    }
}

/// The `text` container of a block map.
fn text_container(map: &LoroMap) -> Result<LoroText, DocCrdtError> {
    match map.get("text") {
        Some(ValueOrContainer::Container(c)) => c
            .into_text()
            .map_err(|_| DocCrdtError::Shape("text field is not a text container".into())),
        _ => Err(DocCrdtError::Shape("block missing text container".into())),
    }
}

/// Write a whole block (id + content) into a fresh block map.
fn write_block(map: &LoroMap, block: &DocumentBlock) -> Result<(), DocCrdtError> {
    map.insert("id", block.id.as_str())?;
    // Create the text container up-front so every block map has a uniform shape.
    let text: LoroText = map.insert_container("text", LoroText::new())?;
    let _ = text;
    write_content(map, block)?;
    Ok(())
}

/// Write a block's content (type + text + meta) into its existing map, replacing
/// the text container's contents and the `meta`/`type` scalars.
fn write_content(map: &LoroMap, block: &DocumentBlock) -> Result<(), DocCrdtError> {
    let (kind, text, meta) = content_parts(&block.content);
    map.insert("type", kind)?;
    map.insert("meta", serde_json::to_string(&meta)?.as_str())?;
    let container = text_container(map)?;
    let len = container.len_unicode();
    if len > 0 {
        container.delete(0, len)?;
    }
    if !text.is_empty() {
        container.insert(0, &text)?;
    }
    Ok(())
}

/// Reconstruct a [`DocumentBlock`] from its CRDT map.
fn read_block(map: &LoroMap) -> Result<DocumentBlock, DocCrdtError> {
    let id =
        string_field(map, "id").ok_or_else(|| DocCrdtError::Shape("block missing id".into()))?;
    let kind = string_field(map, "type")
        .ok_or_else(|| DocCrdtError::Shape("block missing type".into()))?;
    let text = text_container(map)?.to_string();
    let meta: serde_json::Value = match string_field(map, "meta") {
        Some(raw) => serde_json::from_str(&raw)?,
        None => json!({}),
    };
    let content = content_from_parts(&kind, text, &meta)?;
    Ok(DocumentBlock::with_id(id, content))
}

/// Split a block content into (discriminant, primary text, non-text meta).
fn content_parts(content: &BlockContent) -> (&'static str, String, serde_json::Value) {
    match content {
        BlockContent::Heading { level, text } => {
            ("heading", text.clone(), json!({ "level": level }))
        }
        BlockContent::Paragraph { text } => ("paragraph", text.clone(), json!({})),
        BlockContent::Code { language, text } => {
            ("code", text.clone(), json!({ "language": language }))
        }
        BlockContent::Diagram { format, source } => {
            ("diagram", source.clone(), json!({ "format": format }))
        }
        BlockContent::Table { rows } => ("table", String::new(), json!({ "rows": rows })),
        BlockContent::Callout { kind, text } => {
            ("callout", text.clone(), json!({ "callout_kind": kind }))
        }
        BlockContent::Checklist { items } => {
            ("checklist", String::new(), json!({ "items": items }))
        }
        BlockContent::Query { query } => ("query", query.clone(), json!({})),
        BlockContent::EmbeddedFile { path } => {
            ("embedded_file", String::new(), json!({ "path": path }))
        }
        BlockContent::EmbeddedSymbol { symbol } => (
            "embedded_symbol",
            String::new(),
            json!({ "symbol": symbol }),
        ),
        BlockContent::EmbeddedWorkflow { workflow } => (
            "embedded_workflow",
            String::new(),
            json!({ "workflow": workflow }),
        ),
        BlockContent::EmbeddedSkill { skill } => {
            ("embedded_skill", String::new(), json!({ "skill": skill }))
        }
    }
}

/// Reassemble a block content from its (discriminant, text, meta) parts.
fn content_from_parts(
    kind: &str,
    text: String,
    meta: &serde_json::Value,
) -> Result<BlockContent, DocCrdtError> {
    let s = |k: &str| meta.get(k).and_then(|v| v.as_str()).map(str::to_string);
    Ok(match kind {
        "heading" => BlockContent::Heading {
            level: meta
                .get("level")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(1) as u8,
            text,
        },
        "paragraph" => BlockContent::Paragraph { text },
        "code" => BlockContent::Code {
            language: s("language"),
            text,
        },
        "diagram" => BlockContent::Diagram {
            format: s("format").unwrap_or_default(),
            source: text,
        },
        "table" => BlockContent::Table {
            rows: serde_json::from_value(meta.get("rows").cloned().unwrap_or(json!([])))?,
        },
        "callout" => BlockContent::Callout {
            kind: s("callout_kind").unwrap_or_default(),
            text,
        },
        "checklist" => BlockContent::Checklist {
            items: parse_items(meta)?,
        },
        "query" => BlockContent::Query { query: text },
        "embedded_file" => BlockContent::EmbeddedFile {
            path: s("path").unwrap_or_default(),
        },
        "embedded_symbol" => BlockContent::EmbeddedSymbol {
            symbol: s("symbol").unwrap_or_default(),
        },
        "embedded_workflow" => BlockContent::EmbeddedWorkflow {
            workflow: s("workflow").unwrap_or_default(),
        },
        "embedded_skill" => BlockContent::EmbeddedSkill {
            skill: s("skill").unwrap_or_default(),
        },
        other => return Err(DocCrdtError::Shape(format!("unknown block type: {other}"))),
    })
}

/// Parse a checklist's items out of the meta blob.
fn parse_items(meta: &serde_json::Value) -> Result<Vec<ChecklistItem>, DocCrdtError> {
    Ok(serde_json::from_value(
        meta.get("items").cloned().unwrap_or(json!([])),
    )?)
}
