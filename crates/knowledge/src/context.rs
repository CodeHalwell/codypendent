//! The agent-context assembler (Chapter 05–07, STEP 2.3–2.5 seam).
//!
//! The individual fabric surfaces — the [`repository_map`](crate::repomap), the
//! hybrid [`retrieve`] funnel, and the scoped [`MemoryStore`] — each answer one
//! question well. This module folds all three into the single artifact a run
//! actually consumes: a [`ContextManifest`] whose [`render`](ContextManifest::render)
//! is the text block that opens a run's trace, satisfying the Phase-2 exit
//! criterion "agent context includes repository map + cited memories + retrieved
//! tool/skill cards".
//!
//! It is a pool-driven read: it never writes authority. Like the fabric's other
//! managers it is stateless — everything flows from the `pool` and the request —
//! and it projects the rich knowledge types down to the plain, serde-friendly
//! `Context*` shapes so a consumer (the daemon's run executor, a UI) never has to
//! name the retrieval/memory/graph internals to display a manifest.

use std::fmt::Write as _;

use codypendent_protocol::RepositoryId;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::codegraph::CodeGraphError;
use crate::memory::{MemoryError, MemoryStore};
use crate::registry::{Registry, RegistryError};
use crate::retrieval::{
    retrieve, HashingEmbedder, RetrievalConfig, RetrievalError, RetrievalIndexes, RetrievalQuery,
};
use crate::types::{EvidenceRef, MemoryRecord, RiskClass, Scope, ToolCard};

/// A failure assembling the context manifest. Each underlying fabric error is
/// wrapped so the caller can log a cause without matching on the internals.
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    /// Listing the registry (the retrieval authority) failed.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// Building the derived indexes or running the funnel failed.
    #[error(transparent)]
    Retrieval(#[from] RetrievalError),
    /// Querying the memory ledger failed.
    #[error(transparent)]
    Memory(#[from] MemoryError),
    /// Folding the code graph into the repository map failed.
    #[error(transparent)]
    CodeGraph(#[from] CodeGraphError),
}

/// A compact progressive-disclosure card, flattened from a [`ToolCard`] to the
/// fields a manifest displays. Plain data, so a consumer never depends on the
/// registry types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCard {
    /// The item's stable name (e.g. `shell.run`).
    pub name: String,
    /// Its one-line description (the card summary).
    pub summary: String,
    /// Its coarse risk class, shown so a reader sees a behaviour's cost at a
    /// glance.
    pub risk: RiskClass,
}

impl ContextCard {
    /// Project a disclosed [`ToolCard`] into a manifest card.
    #[must_use]
    fn from_card(card: &ToolCard) -> Self {
        Self {
            name: card.name.clone(),
            summary: card.summary.clone(),
            risk: card.risk,
        }
    }
}

/// A cited memory, flattened from a [`MemoryRecord`] to a statement plus a
/// human-readable pointer back at its first piece of evidence — enough for a
/// reader to see the fact and where it came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextMemory {
    /// The curated statement.
    pub statement: String,
    /// A human string naming the first [`EvidenceRef`] the memory cites (the
    /// source a client can open); `"(no evidence)"` never occurs for a stored
    /// memory, which the curator guarantees carries provenance.
    pub source: String,
    /// The revision the memory is valid from.
    pub revision: String,
    /// The curator's confidence in the fact, in `[0, 1]`.
    pub confidence: f32,
}

impl ContextMemory {
    /// Project a stored [`MemoryRecord`] into a manifest memory, naming its first
    /// evidence ref as the source.
    #[must_use]
    fn from_record(record: &MemoryRecord) -> Self {
        Self {
            statement: record.statement.clone(),
            source: format_source(record.provenance.first()),
            revision: record.valid_from.0.clone(),
            confidence: record.confidence,
        }
    }
}

/// Everything a run's context opens with: the repository map, the disclosed
/// tool/skill cards, and the cited memories in scope. All fields are plain data;
/// [`render`](ContextManifest::render) turns them into the trace text block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextManifest {
    /// The rendered repository map (packages → modules → APIs → tests).
    pub repository_map: String,
    /// The disclosed tool cards (6–12, progressive disclosure).
    pub tool_cards: Vec<ContextCard>,
    /// The disclosed skill cards (1–3).
    pub skill_cards: Vec<ContextCard>,
    /// The memories cited from the requested scopes, each with its source.
    pub memories: Vec<ContextMemory>,
}

impl ContextManifest {
    /// Render the manifest as a compact, three-section text block — the exact
    /// representation a run's trace shows. The sections are always present (even
    /// when empty) so the block is stable to read and grep.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();

        let _ = writeln!(out, "=== REPOSITORY MAP ===");
        if self.repository_map.trim().is_empty() {
            let _ = writeln!(out, "(empty)");
        } else {
            let _ = write!(out, "{}", self.repository_map);
            if !self.repository_map.ends_with('\n') {
                let _ = writeln!(out);
            }
        }

        let _ = writeln!(out, "\n=== TOOLS ===");
        if self.tool_cards.is_empty() && self.skill_cards.is_empty() {
            let _ = writeln!(out, "(none)");
        } else {
            for card in &self.tool_cards {
                let _ = writeln!(
                    out,
                    "tool {} [{}] — {}",
                    card.name,
                    risk_label(card.risk),
                    card.summary
                );
            }
            for card in &self.skill_cards {
                let _ = writeln!(
                    out,
                    "skill {} [{}] — {}",
                    card.name,
                    risk_label(card.risk),
                    card.summary
                );
            }
        }

        let _ = writeln!(out, "\n=== MEMORIES ===");
        if self.memories.is_empty() {
            let _ = writeln!(out, "(none)");
        } else {
            for memory in &self.memories {
                let _ = writeln!(
                    out,
                    "- {} (confidence {:.2}, rev {}; source: {})",
                    memory.statement, memory.confidence, memory.revision, memory.source
                );
            }
        }

        out
    }
}

/// Assemble the [`ContextManifest`] a run opens with, reading the fabric through
/// the `pool`.
///
/// The three sections are folded from their authoritative surfaces:
/// - **repository map** — [`repository_map`](crate::repomap::repository_map) over
///   the persisted code graph for `repository`;
/// - **tool/skill cards** — the hybrid [`retrieve`] funnel over
///   [`Registry::list`], asking for `objective` under the caller's `scopes`
///   (always widened to include [`System`](Scope::System) and `repository`) with
///   a [`Medium`](RiskClass::Medium) risk ceiling, so destructive behaviours are
///   filtered, never merely down-ranked;
/// - **memories** — [`MemoryStore::query`] over exactly the requested `scopes`
///   (cross-repository isolation is the SQL filter, never a heuristic), each
///   projected with a human-readable pointer at its first evidence ref.
pub async fn assemble_context(
    pool: &SqlitePool,
    repository: RepositoryId,
    objective: &str,
    scopes: &[Scope],
) -> Result<ContextManifest, ContextError> {
    // 1. Repository map — fold the persisted graph into the compact tree.
    let repository_map = crate::repomap::repository_map(pool, repository)
        .await?
        .render();

    // 2. Retrieve the disclosed tool + skill cards over the whole registry.
    let items = Registry::new().list(pool).await?;
    let indexes = RetrievalIndexes::build(&items, HashingEmbedder::new())?;
    let query = RetrievalQuery::new(
        objective,
        visible_scopes(repository, scopes),
        RiskClass::Medium,
    );
    let result = retrieve(&items, &indexes, &query, &RetrievalConfig::default())?;
    let tool_cards = result.tools.iter().map(ContextCard::from_card).collect();
    let skill_cards = result.skills.iter().map(ContextCard::from_card).collect();

    // 3. Cited memories in the requested scopes (currently-live view), capped.
    // The 2.3 funnel budgets tool/skill disclosure; without a ceiling here the
    // memory section regrew the exact failure mode that budget exists to
    // prevent — every live memory of a long-lived repository, unbounded. The
    // store returns oldest-first, so keeping the tail keeps the newest.
    let records = MemoryStore::new().query(pool, scopes, None).await?;
    let dropped = records.len().saturating_sub(MAX_CONTEXT_MEMORIES);
    if dropped > 0 {
        tracing::debug!(
            dropped,
            kept = MAX_CONTEXT_MEMORIES,
            "context memory ceiling applied (newest kept)"
        );
    }
    let memories = records
        .iter()
        .skip(dropped)
        .map(ContextMemory::from_record)
        .collect();

    Ok(ContextManifest {
        repository_map,
        tool_cards,
        skill_cards,
        memories,
    })
}

/// Ceiling on memories injected into one run context (newest survive). Chosen
/// to keep the memory section within the same order of magnitude as the
/// disclosed tool/skill cards; retrieval-ranked memory selection is Phase 7+
/// territory — until then recency is the only defensible ordering.
const MAX_CONTEXT_MEMORIES: usize = 32;

/// The visibility chain the retrieval funnel filters against: the caller's
/// `scopes`, always widened with [`System`](Scope::System) (built-ins live there)
/// and the active `repository` (so repository-scoped skills are visible), deduped.
fn visible_scopes(repository: RepositoryId, scopes: &[Scope]) -> Vec<Scope> {
    let mut visible: Vec<Scope> = scopes.to_vec();
    for required in [Scope::System, Scope::Repository(repository)] {
        if !visible.contains(&required) {
            visible.push(required);
        }
    }
    visible
}

/// A human-readable name for the first evidence ref a memory cites — the "source"
/// a client renders and can open. `None` (an unreachable case for a stored
/// memory) renders as `"(no evidence)"`.
fn format_source(evidence: Option<&EvidenceRef>) -> String {
    match evidence {
        Some(EvidenceRef::EventRange {
            session_id,
            from_sequence,
            to_sequence,
        }) => format!("session {session_id} events {from_sequence}–{to_sequence}"),
        Some(EvidenceRef::Artifact {
            artifact,
            source_path,
        }) => match source_path {
            Some(path) => format!("artifact {} ({path})", artifact.id),
            None => format!("artifact {}", artifact.id),
        },
        None => "(no evidence)".to_string(),
    }
}

/// A short label for a [`RiskClass`] used in [`ContextManifest::render`].
fn risk_label(risk: RiskClass) -> &'static str {
    match risk {
        RiskClass::Safe => "safe",
        RiskClass::Low => "low",
        RiskClass::Medium => "medium",
        RiskClass::High => "high",
    }
}
