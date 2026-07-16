//! Hybrid retrieval — the Chapter 05 funnel (STEP 2.3).
//!
//! Given a task, the registry retrieves a broad candidate set from four
//! complementary sources, removes anything the caller is not allowed to see with
//! **hard filters** (security is a filter, never a score), reranks the survivors
//! by a weighted sum, pulls a selected skill's required tools in by dependency
//! closure, and discloses a compact set of [`ToolCard`]s within a context budget:
//!
//! ```text
//! candidates = dense top N ∪ BM25 top N ∪ exact id/keyword/intent top N ∪ history top N
//!   → hard filters: scope chain, minimum trust, drop non-executable behaviours,
//!                    drop risk above the query's ceiling   (security is a FILTER)
//!   → rerank: dense + lexical + exact + dependency + trust_bonus − risk_penalty
//!   → dependency closure: a selected skill pulls its required tools by name
//!   → context budget: 6–12 tool cards + 1–3 skill cards
//! ```
//!
//! Only compact cards cross into context (progressive disclosure); the full JSON
//! schemas load later, only for the items the model actually selects. The
//! [`RetrievalTrace`] records the config version, the candidate union, and the
//! final selection so a run is auditable and Phase-7 tuning is measurable.

mod bm25;
mod config;
mod embed;
mod vector;

use std::collections::{HashMap, HashSet};

use codypendent_protocol::RegistryItemId;

use crate::types::{RegistryItem, RegistryItemKind, RiskClass, Scope, ToolCard, TrustTier};

pub use bm25::{Bm25Error, Bm25Index};
pub use config::{RerankWeights, RetrievalConfig};
pub use embed::{Embedder, HashingEmbedder, EMBEDDING_DIMENSION};
pub use vector::VectorIndex;

/// A failure building the derived indexes or running retrieval.
#[derive(Debug, thiserror::Error)]
pub enum RetrievalError {
    /// The lexical index failed to build or query.
    #[error(transparent)]
    Bm25(#[from] Bm25Error),
}

/// A retrieval request: the task text, the caller's visibility, and the security
/// ceilings the hard filters enforce.
#[derive(Debug, Clone)]
pub struct RetrievalQuery {
    /// The natural-language task.
    pub text: String,
    /// The scope chain the caller may see. An item is visible if its scope is
    /// [`System`](Scope::System) (always) or appears in this list — this is how
    /// cross-repository isolation is enforced structurally, never by heuristic.
    pub visible_scopes: Vec<Scope>,
    /// The highest [`RiskClass`] the caller will accept; anything above it is
    /// filtered out (this is how forbidden/destructive items are excluded).
    pub risk_ceiling: RiskClass,
    /// The minimum provenance [`TrustTier`] to admit.
    pub min_trust: TrustTier,
    /// Ids of items that recently succeeded on similar tasks (history source);
    /// empty when no history is available.
    pub history: Vec<RegistryItemId>,
}

impl RetrievalQuery {
    /// A query with the common defaults: no history, [`TrustTier::Untrusted`]
    /// floor (admit every tier), and the given text/scopes/ceiling.
    #[must_use]
    pub fn new(
        text: impl Into<String>,
        visible_scopes: Vec<Scope>,
        risk_ceiling: RiskClass,
    ) -> Self {
        Self {
            text: text.into(),
            visible_scopes,
            risk_ceiling,
            min_trust: TrustTier::Untrusted,
            history: Vec::new(),
        }
    }
}

/// The disclosed context package: compact cards only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievalResult {
    /// Disclosed tool cards (6–12), including any pulled in by dependency closure.
    pub tools: Vec<ToolCard>,
    /// Disclosed skill cards (1–3).
    pub skills: Vec<ToolCard>,
    /// The auditable record of how this selection was produced.
    pub trace: RetrievalTrace,
}

/// The audit trail for one retrieval (Phase-7 tuning reads this back).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievalTrace {
    /// The [`RetrievalConfig::version`] that produced this selection.
    pub config_version: u32,
    /// Every candidate id in the pre-filter union, in source order.
    pub candidate_ids: Vec<RegistryItemId>,
    /// The disclosed ids (tools then skills), in disclosure order.
    pub selected_ids: Vec<RegistryItemId>,
}

/// The derived indexes plus the embedder that built them — everything retrieval
/// needs beyond the authoritative items. Rebuild from
/// [`Registry::list`](crate::registry::Registry::list) whenever authority changes
/// (the outbox drives this in production).
pub struct RetrievalIndexes {
    /// The dense (cosine) index.
    pub vector: VectorIndex,
    /// The lexical (BM25) index.
    pub bm25: Bm25Index,
    /// The embedder, retained to embed queries at run time with the same model
    /// (and content-hash cache) that embedded the items.
    embedder: Box<dyn Embedder + Send + Sync>,
}

impl std::fmt::Debug for RetrievalIndexes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetrievalIndexes")
            .field("vector", &self.vector)
            .field("bm25", &self.bm25)
            .finish_non_exhaustive()
    }
}

impl RetrievalIndexes {
    /// Build both derived indexes over `items` using `embedder`, retaining the
    /// embedder for query time.
    pub fn build<E>(items: &[RegistryItem], embedder: E) -> Result<Self, RetrievalError>
    where
        E: Embedder + Send + Sync + 'static,
    {
        let mut vector = VectorIndex::new();
        for item in items {
            vector.insert(item.id, embedder.embed(&embedding_text(item)));
        }
        let bm25 = Bm25Index::build(items)?;
        Ok(Self {
            vector,
            bm25,
            embedder: Box::new(embedder),
        })
    }
}

/// The text a registry item is embedded from: its name, description, and intents
/// (Chapter 05 — "embed registry descriptions + intents").
#[must_use]
pub fn embedding_text(item: &RegistryItem) -> String {
    format!(
        "{} {} {}",
        item.name,
        item.description,
        item.intents.join(" ")
    )
}

/// Run the full funnel and disclose the context package.
///
/// `items` is the authoritative set (typically [`Registry::list`](crate::registry::Registry::list));
/// `indexes` are the derived indexes built over the same set; `config` is the
/// versioned tuning. Returns the disclosed cards and the trace.
pub fn retrieve(
    items: &[RegistryItem],
    indexes: &RetrievalIndexes,
    query: &RetrievalQuery,
    config: &RetrievalConfig,
) -> Result<RetrievalResult, RetrievalError> {
    let by_id: HashMap<RegistryItemId, &RegistryItem> =
        items.iter().map(|item| (item.id, item)).collect();

    // ---- Candidate union: four complementary sources ------------------------
    let query_vec = indexes.embedder.embed(&query.text);
    let dense = indexes.vector.search(&query_vec, config.dense_top);
    let lexical = indexes.bm25.search(&query.text, config.bm25_top)?;
    let exact = exact_match(items, &query.text, config.exact_top);

    let dense_scores: HashMap<RegistryItemId, f32> = dense.iter().copied().collect();
    let lexical_scores: HashMap<RegistryItemId, f32> = lexical.iter().copied().collect();
    let exact_scores: HashMap<RegistryItemId, f32> = exact.iter().copied().collect();

    let mut candidate_ids: Vec<RegistryItemId> = Vec::new();
    let mut seen: HashSet<RegistryItemId> = HashSet::new();
    let push =
        |id: RegistryItemId, seen: &mut HashSet<RegistryItemId>, out: &mut Vec<RegistryItemId>| {
            if seen.insert(id) {
                out.push(id);
            }
        };
    for (id, _) in &dense {
        push(*id, &mut seen, &mut candidate_ids);
    }
    for (id, _) in &lexical {
        push(*id, &mut seen, &mut candidate_ids);
    }
    for (id, _) in &exact {
        push(*id, &mut seen, &mut candidate_ids);
    }
    for id in query.history.iter().take(config.history_top) {
        if by_id.contains_key(id) {
            push(*id, &mut seen, &mut candidate_ids);
        }
    }

    // ---- Hard filters: security is a FILTER, never a score ------------------
    let survivors: Vec<&RegistryItem> = candidate_ids
        .iter()
        .filter_map(|id| by_id.get(id).copied())
        .filter(|item| passes_hard_filters(item, query))
        .collect();

    // Normalizers so each signal lands in ~[0, 1] before the weighted sum.
    let max_lexical = lexical_scores.values().copied().fold(0.0_f32, f32::max);
    let max_exact = exact_scores.values().copied().fold(0.0_f32, f32::max);

    // Dependency signal: a tool is boosted when a lexically-relevant skill
    // requires it (the "graph dependency relevance" term). Built from the
    // surviving skills so a filtered-out skill never boosts anything.
    let dependency_scores = dependency_signal(&survivors, &lexical_scores);

    // ---- Rerank: weighted sum of the signals --------------------------------
    let weights = &config.weights;
    let mut ranked: Vec<Scored> = survivors
        .iter()
        .map(|item| {
            let dense_sig = dense_scores.get(&item.id).copied().unwrap_or(0.0).max(0.0);
            let lexical_sig = normalize(
                lexical_scores.get(&item.id).copied().unwrap_or(0.0),
                max_lexical,
            );
            let exact_sig = normalize(
                exact_scores.get(&item.id).copied().unwrap_or(0.0),
                max_exact,
            );
            let dependency_sig = dependency_scores.get(&item.id).copied().unwrap_or(0.0);
            let trust_sig = trust_signal(item.trust.tier);
            let risk_sig = risk_signal(item.risk);
            let score = weights.dense * dense_sig
                + weights.lexical * lexical_sig
                + weights.exact * exact_sig
                + weights.dependency * dependency_sig
                + weights.trust_bonus * trust_sig
                - weights.risk_penalty * risk_sig;
            Scored { item, score }
        })
        .collect();
    ranked.sort_by(|a, b| b.score.total_cmp(&a.score));
    ranked.truncate(config.rerank_pool.max(1));

    // ---- Partition into tools and skills, in rank order ---------------------
    let ranked_tools: Vec<&RegistryItem> = ranked
        .iter()
        .filter(|s| s.item.kind == RegistryItemKind::Tool)
        .map(|s| s.item)
        .collect();
    let ranked_skills: Vec<&RegistryItem> = ranked
        .iter()
        .filter(|s| s.item.kind != RegistryItemKind::Tool)
        .map(|s| s.item)
        .collect();

    // ---- Disclose skills (1–3), bounded by availability ---------------------
    let skill_take = bounded(
        ranked_skills.len(),
        config.disclose_skills_min,
        config.disclose_skills_max,
    );
    let disclosed_skills: Vec<&RegistryItem> = ranked_skills.into_iter().take(skill_take).collect();

    // ---- Dependency closure: a selected skill pulls its required tools ------
    // Required tools are resolved *within the survivors* (by name), so closure
    // can never re-introduce an item the hard filters removed — security holds
    // through the closure too.
    let survivor_tool_by_name: HashMap<&str, &RegistryItem> = survivors
        .iter()
        .filter(|item| item.kind == RegistryItemKind::Tool)
        .map(|item| (item.name.as_str(), *item))
        .collect();
    let mut required_set: HashSet<RegistryItemId> = HashSet::new();
    for skill in &disclosed_skills {
        for dependency in &skill.dependencies {
            if dependency.optional {
                continue;
            }
            if let Some(tool) = survivor_tool_by_name.get(dependency.target.as_str()) {
                required_set.insert(tool.id);
            }
        }
    }

    // ---- Context budget: closure-required tools first, then top-ranked ------
    // Required tools take priority within the tool budget so recall@k stays an
    // honest measure of a k-card disclosure (closure never inflates the count).
    let mut disclosed_tool_ids: Vec<RegistryItemId> = Vec::new();
    let mut tool_set: HashSet<RegistryItemId> = HashSet::new();
    // Required tools ordered by their own rank, not skill-declaration order.
    for scored in ranked.iter().filter(|s| required_set.contains(&s.item.id)) {
        if disclosed_tool_ids.len() >= config.disclose_tools_max {
            break;
        }
        if tool_set.insert(scored.item.id) {
            disclosed_tool_ids.push(scored.item.id);
        }
    }
    for item in ranked_tools {
        if disclosed_tool_ids.len() >= config.disclose_tools_max {
            break;
        }
        if tool_set.insert(item.id) {
            disclosed_tool_ids.push(item.id);
        }
    }

    let tools: Vec<ToolCard> = disclosed_tool_ids
        .iter()
        .filter_map(|id| by_id.get(id).map(|item| ToolCard::of(item)))
        .collect();
    let skills: Vec<ToolCard> = disclosed_skills
        .iter()
        .map(|item| ToolCard::of(item))
        .collect();

    let mut selected_ids = disclosed_tool_ids;
    selected_ids.extend(disclosed_skills.iter().map(|item| item.id));

    Ok(RetrievalResult {
        tools,
        skills,
        trace: RetrievalTrace {
            config_version: config.version,
            candidate_ids,
            selected_ids,
        },
    })
}

/// One reranked candidate and its score.
struct Scored<'a> {
    item: &'a RegistryItem,
    score: f32,
}

/// The four hard filters, in the Chapter 05 order. Any `false` drops the item
/// before it can be scored — a too-risky or out-of-scope item is never merely
/// down-ranked.
fn passes_hard_filters(item: &RegistryItem, query: &RetrievalQuery) -> bool {
    // Scope: System is always visible; every other tier must be in the chain.
    let scope_visible = item.scope == Scope::System || query.visible_scopes.contains(&item.scope);
    if !scope_visible {
        return false;
    }
    // Minimum trust tier.
    if item.trust.tier < query.min_trust {
        return false;
    }
    // Drop non-executable *behaviours*: a tool that cannot be invoked is never
    // selectable. A skill is disclosed for its procedure (its bundled scripts
    // simply wait for the Phase-6 sandbox) and contributes only *executable*
    // tools via dependency closure, so it is not dropped for non-executability.
    if item.kind == RegistryItemKind::Tool && !item.executable {
        return false;
    }
    // Risk ceiling: this is the filter that excludes destructive/forbidden items.
    if item.risk > query.risk_ceiling {
        return false;
    }
    true
}

/// Score every item by exact token overlap with the query and return the
/// `top_k`. A tool whose name/keyword/intent tokens the user typed verbatim is a
/// strong, precise signal — this both seeds the candidate union and feeds the
/// `exact` rerank term.
fn exact_match(items: &[RegistryItem], query: &str, top_k: usize) -> Vec<(RegistryItemId, f32)> {
    let query_tokens: HashSet<String> = tokenize(query).into_iter().collect();
    let mut scored: Vec<(RegistryItemId, f32)> = items
        .iter()
        .map(|item| (item.id, exact_score(item, &query_tokens)))
        .filter(|(_, score)| *score > 0.0)
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(top_k);
    scored
}

/// The exact-overlap score for one item: the count of distinct query tokens that
/// appear among the item's tokens, with a name-token match counted double (the
/// stable id is the most deliberate thing a user can name).
fn exact_score(item: &RegistryItem, query_tokens: &HashSet<String>) -> f32 {
    let name_tokens: HashSet<String> = tokenize(&item.name).into_iter().collect();
    let mut other_tokens: HashSet<String> = HashSet::new();
    for keyword in &item.keywords {
        other_tokens.extend(tokenize(keyword));
    }
    for intent in &item.intents {
        other_tokens.extend(tokenize(intent));
    }

    let mut score = 0.0_f32;
    for token in query_tokens {
        if name_tokens.contains(token) {
            score += 2.0;
        } else if other_tokens.contains(token) {
            score += 1.0;
        }
    }
    score
}

/// The dependency-relevance signal: a tool required by a surviving skill that
/// itself matched the query lexically is boosted by that skill's lexical
/// strength. On a task with no relevant skill (a plain "show me the diff"), no
/// tool is boosted, so the term never fires spuriously.
fn dependency_signal(
    survivors: &[&RegistryItem],
    lexical_scores: &HashMap<RegistryItemId, f32>,
) -> HashMap<RegistryItemId, f32> {
    let max_lexical = lexical_scores.values().copied().fold(0.0_f32, f32::max);
    let tool_id_by_name: HashMap<&str, RegistryItemId> = survivors
        .iter()
        .filter(|item| item.kind == RegistryItemKind::Tool)
        .map(|item| (item.name.as_str(), item.id))
        .collect();

    let mut signal: HashMap<RegistryItemId, f32> = HashMap::new();
    for skill in survivors
        .iter()
        .filter(|item| item.kind != RegistryItemKind::Tool)
    {
        let skill_relevance = normalize(
            lexical_scores.get(&skill.id).copied().unwrap_or(0.0),
            max_lexical,
        );
        if skill_relevance <= 0.0 {
            continue;
        }
        for dependency in &skill.dependencies {
            if dependency.optional {
                continue;
            }
            if let Some(tool_id) = tool_id_by_name.get(dependency.target.as_str()) {
                let entry = signal.entry(*tool_id).or_insert(0.0);
                *entry = entry.max(skill_relevance);
            }
        }
    }
    signal
}

/// Lowercase alphanumeric tokens, splitting on everything else (so
/// `workspace.read_file` → `["workspace", "read", "file"]`).
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Divide by `max` into `[0, 1]`; `0.0` when `max` is non-positive.
fn normalize(value: f32, max: f32) -> f32 {
    if max > 0.0 {
        (value / max).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Provenance trust as a `[0, 1]` bonus signal.
fn trust_signal(tier: TrustTier) -> f32 {
    match tier {
        TrustTier::Untrusted => 0.0,
        TrustTier::Community => 1.0 / 3.0,
        TrustTier::Verified => 2.0 / 3.0,
        TrustTier::FirstParty => 1.0,
    }
}

/// Coarse risk as a `[0, 1]` penalty signal.
fn risk_signal(risk: RiskClass) -> f32 {
    match risk {
        RiskClass::Safe => 0.0,
        RiskClass::Low => 1.0 / 3.0,
        RiskClass::Medium => 2.0 / 3.0,
        RiskClass::High => 1.0,
    }
}

/// How many to disclose from `available`, honouring the `[min, max]` budget: take
/// up to `max`, but never fewer than `min` when that many are available.
fn bounded(available: usize, min: usize, max: usize) -> usize {
    available.min(max).max(min.min(available))
}
