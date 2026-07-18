//! The documentation staleness engine (STEP 4.6).
//!
//! Documents embed `{{ symbol:path::to::symbol }}` references (as
//! [`BlockContent::EmbeddedSymbol`] blocks or inline markers in text). The
//! publisher **resolves** each against the code graph, recording the resolved
//! symbol identity and signature at that revision on a [`DocumentLink`]. When the
//! graph later changes, [`detect_staleness`] diffs the recorded links against the
//! live graph: a **signature change** or a **disappearance** emits a
//! [`StalenessFinding`] carrying the evidence (the before/after signature and the
//! causing revision) and a suggested review scope. The `Maintain` collaboration
//! mode consumes findings by drafting a *suggestion* — never a direct edit —
//! citing the code change; that flow is registered as the `/update-docs` command.

use codypendent_protocol::{DocumentId, RepositoryId};
use sqlx::SqlitePool;

use crate::codegraph::{self, CodeGraphError, SymbolSnapshot};
use crate::types::GitRevision;

use super::collab::NewSuggestion;
use super::model::{
    BlockContent, DocumentAuthor, DocumentBlock, DocumentLink, DocumentRelation, LinkTarget,
    ResolvedSymbol,
};

/// A symbol referenced by a document, with the block it appears in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRef {
    pub block_id: Option<String>,
    pub qualified_name: String,
}

/// Extract every `{{ symbol:… }}` reference in a document — both dedicated
/// [`BlockContent::EmbeddedSymbol`] blocks and inline markers in text blocks.
#[must_use]
pub fn symbol_references(blocks: &[DocumentBlock]) -> Vec<SymbolRef> {
    let mut refs = Vec::new();
    for block in blocks {
        if let BlockContent::EmbeddedSymbol { symbol } = &block.content {
            refs.push(SymbolRef {
                block_id: Some(block.id.clone()),
                qualified_name: symbol.trim().to_string(),
            });
        }
        if let Some(text) = block.primary_text() {
            for name in inline_symbol_markers(text) {
                refs.push(SymbolRef {
                    block_id: Some(block.id.clone()),
                    qualified_name: name,
                });
            }
        }
    }
    refs
}

/// Find inline `{{ symbol:NAME }}` markers in free text.
fn inline_symbol_markers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{ symbol:") {
        let after = &rest[start + "{{ symbol:".len()..];
        if let Some(end) = after.find("}}") {
            let name = after[..end].trim().to_string();
            if !name.is_empty() {
                out.push(name);
            }
            rest = &after[end + 2..];
        } else {
            break;
        }
    }
    out
}

/// Resolve a document's symbol references against the code graph at `revision`,
/// producing `References → Symbol` links each carrying the resolved
/// [`ResolvedSymbol`] (stable key + signature at resolution) — the snapshot
/// staleness later compares against. Unresolved references still produce a link
/// with `resolved = None`.
pub async fn resolve_links(
    pool: &SqlitePool,
    repository: RepositoryId,
    blocks: &[DocumentBlock],
    revision: &GitRevision,
) -> Result<Vec<DocumentLink>, CodeGraphError> {
    let nodes = codegraph::nodes(pool, repository).await?;
    let mut links = Vec::new();
    for reference in symbol_references(blocks) {
        let node = nodes
            .iter()
            .find(|n| n.key.qualified_name == reference.qualified_name);
        let resolved = node.map(|n| ResolvedSymbol {
            symbol_key: n.key.stable_key(),
            signature_hash: n.key.signature_hash.clone().map(|h| h.0),
            revision: revision.0.clone(),
        });
        links.push(DocumentLink {
            relation: DocumentRelation::References,
            target: LinkTarget::Symbol(reference.qualified_name),
            block_id: reference.block_id,
            resolved,
        });
    }
    Ok(links)
}

/// Why a linked symbol is stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StalenessReason {
    /// The symbol's signature changed since the document was resolved.
    SignatureChanged,
    /// The symbol no longer exists in the graph.
    Disappeared,
}

/// A staleness finding: a document's symbol link no longer matches the live
/// graph, with the evidence (before/after signature + causing revision) and a
/// suggested scope of review (Chapter 07 / exit criterion 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalenessFinding {
    pub document_id: DocumentId,
    pub block_id: Option<String>,
    pub qualified_name: String,
    pub reason: StalenessReason,
    /// The signature recorded when the link was resolved.
    pub before_signature: Option<String>,
    /// The signature in the live graph (`None` when the symbol disappeared).
    pub after_signature: Option<String>,
    /// The revision whose change produced the finding (the causing commit).
    pub revision: String,
    /// A human-readable suggested scope of review.
    pub review_scope: String,
}

/// Diff a document's resolved links against the current graph snapshot, emitting
/// a finding per link whose symbol changed signature or disappeared. Links whose
/// symbol is unchanged (or that never resolved) produce nothing. `after_revision`
/// labels the causing commit in the evidence.
#[must_use]
pub fn detect_staleness(
    document_id: DocumentId,
    links: &[DocumentLink],
    current: &[SymbolSnapshot],
    after_revision: &GitRevision,
) -> Vec<StalenessFinding> {
    let mut findings = Vec::new();
    for link in links {
        let (LinkTarget::Symbol(name), Some(resolved)) = (&link.target, &link.resolved) else {
            continue;
        };
        let live = current.iter().find(|s| &s.qualified_name == name);
        match live {
            None => findings.push(StalenessFinding {
                document_id,
                block_id: link.block_id.clone(),
                qualified_name: name.clone(),
                reason: StalenessReason::Disappeared,
                before_signature: resolved.signature_hash.clone(),
                after_signature: None,
                revision: after_revision.0.clone(),
                review_scope: format!("{name} was removed — review the referencing section"),
            }),
            Some(sym) if sym.signature_hash != resolved.signature_hash => {
                findings.push(StalenessFinding {
                    document_id,
                    block_id: link.block_id.clone(),
                    qualified_name: name.clone(),
                    reason: StalenessReason::SignatureChanged,
                    before_signature: resolved.signature_hash.clone(),
                    after_signature: sym.signature_hash.clone(),
                    revision: after_revision.0.clone(),
                    review_scope: format!(
                        "{name} signature changed ({}) — review the referencing section",
                        sym.source_path
                    ),
                });
            }
            Some(_) => {}
        }
    }
    findings
}

impl StalenessFinding {
    /// Draft a `Maintain`-mode suggestion for this finding: a *proposed* note (never
    /// a direct edit) on the stale block, citing the causing revision. The
    /// suggestion inserts at the start of the block (an empty range) so nothing is
    /// overwritten until a human accepts it.
    #[must_use]
    pub fn as_suggestion(&self, author: DocumentAuthor) -> Option<NewSuggestion> {
        let block_id = self.block_id.clone()?;
        let cause = match self.reason {
            StalenessReason::SignatureChanged => "signature changed",
            StalenessReason::Disappeared => "was removed",
        };
        Some(NewSuggestion {
            block_id,
            range_start: 0,
            range_end: 0,
            replacement: format!(
                "> [!WARNING] `{}` {cause} in {} — review this section.\n",
                self.qualified_name, self.revision
            ),
            author,
            rationale: Some(format!(
                "staleness: {} {cause} at revision {} ({})",
                self.qualified_name, self.revision, self.review_scope
            )),
        })
    }
}
