//! Hybrid retrieval — the Chapter 05 funnel (STEP 2.3).
//!
//! Given a task, the registry applies visibility and governance filters before
//! collecting bounded candidates from four complementary sources, reranks them
//! by a weighted sum, pulls a selected skill's required tools in by dependency
//! closure, and discloses a compact set of [`ToolCard`]s within a context budget:
//!
//! ```text
//! hard filters: scope, trust, executable tools, risk, status, scope overrides
//!   → candidates = dense top N ∪ BM25 top N ∪ exact top N ∪ history top N
//!   → rerank: dense + lexical + exact + dependency + trust_bonus − risk_penalty
//!   → dependency closure: a selected skill pulls its required tools by name
//!   → context budget: 6–12 tool cards + 1–3 skill cards + 0–2 command cards
//! ```
//!
//! Only compact cards cross into context (progressive disclosure); the full JSON
//! schemas load later, only for the items the model actually selects. The
//! [`RetrievalTrace`] records the config version, the candidate union, and the
//! final selection so a run is auditable and Phase-7 tuning is measurable.

mod bm25;
mod config;
mod embed;
mod persist;
mod vector;

use std::collections::{HashMap, HashSet};

use codypendent_protocol::RegistryItemId;

use crate::types::{
    RegistryItem, RegistryItemKind, RegistryStatus, RiskClass, Scope, ToolCard, TrustTier,
};

pub use bm25::{Bm25Error, Bm25Index};
pub use config::{RerankWeights, RetrievalConfig};
pub use embed::{EmbedError, Embedder, HashingEmbedder, SemanticEmbedder, EMBEDDING_DIMENSION};
pub use persist::{
    drain_outbox, embedding_content_hash, load_embeddings, reconcile_embeddings, semantic_indexes,
    upsert_embedding, DrainReport, PersistError, StoredEmbedding,
};
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
    /// A pre-computed dense vector for [`text`](Self::text), in the SAME space
    /// as the indexes' [`VectorIndex`]. `None` (the default) embeds the query
    /// through the indexes' retained [`Embedder`] — today's behavior. Set by
    /// [`semantic_indexes`] callers, whose item vectors come from an async
    /// remote model the sync funnel cannot call: without the override the query
    /// would be embedded in a different space than the items and dense scoring
    /// would silently degrade to zero.
    pub query_vector: Option<Vec<f32>>,
}

impl RetrievalQuery {
    /// A query with the common defaults: no history, [`TrustTier::Untrusted`]
    /// floor (admit every tier), the retained-embedder query vector, and the
    /// given text/scopes/ceiling.
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
            query_vector: None,
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
    /// Disclosed command cards (0–2) — the slash commands (`/fix-ci`,
    /// `/update-docs`) whose registry rows the funnel used to score and then
    /// discard. A command is not model-callable; disclosing it tells the run
    /// that a whole prepared workflow already exists for this task.
    pub commands: Vec<ToolCard>,
    /// The auditable record of how this selection was produced.
    pub trace: RetrievalTrace,
}

/// The audit trail for one retrieval (Phase-7 tuning reads this back).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievalTrace {
    /// The [`RetrievalConfig::version`] that produced this selection.
    pub config_version: u32,
    /// Every eligible candidate id in the bounded union, in source order.
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

    /// Assemble indexes from an already-built [`VectorIndex`] (persisted /
    /// remote-model vectors), building only BM25 over `items` here. `embedder`
    /// is retained as the query-time fallback; a caller whose vector index came
    /// from a DIFFERENT embedding space must pass the matching query vector via
    /// [`RetrievalQuery::query_vector`] — a fallback-embedded query then scores
    /// dense 0 (cosine of mismatched dims), degrading gracefully rather than
    /// mis-ranking.
    pub fn from_parts<E>(
        items: &[RegistryItem],
        vector: VectorIndex,
        embedder: E,
    ) -> Result<Self, RetrievalError>
    where
        E: Embedder + Send + Sync + 'static,
    {
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
    let eligible = collapse_shadowed(
        items
            .iter()
            .filter(|item| passes_hard_filters(item, query))
            .collect(),
    );
    let allowed: HashSet<_> = eligible.iter().map(|item| item.id).collect();

    // ---- Candidate union: four complementary sources ------------------------
    // A caller-supplied vector (async/remote model, same space as the index)
    // wins; otherwise the retained embedder embeds the query as before.
    let query_vec = match &query.query_vector {
        Some(vector) => vector.clone(),
        None => indexes.embedder.embed(&query.text),
    };
    let dense = indexes
        .vector
        .search_allowed(&query_vec, config.dense_top, &allowed);
    let lexical = indexes
        .bm25
        .search_allowed(&query.text, config.bm25_top, &allowed)?;
    let exact = exact_match(&eligible, &query.text, config.exact_top);

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
    for id in query
        .history
        .iter()
        .filter(|id| allowed.contains(id))
        .take(config.history_top)
    {
        push(*id, &mut seen, &mut candidate_ids);
    }

    let survivors: Vec<&RegistryItem> = candidate_ids
        .iter()
        .filter(|id| allowed.contains(id))
        .filter_map(|id| by_id.get(id).copied())
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
        .filter(|s| s.item.kind == RegistryItemKind::Skill)
        .map(|s| s.item)
        .collect();
    // Commands were the third kind all along: they pass the hard filters, enter
    // the candidate union, and occupy rerank-pool slots — and used to be dropped
    // here, unpartitioned, so `/fix-ci` could never reach a run however exactly
    // the query matched its intents. Budgeted separately (see
    // `disclose_commands_max`) so surfacing one never costs a tool card.
    let disclosed_commands: Vec<&RegistryItem> = ranked
        .iter()
        .filter(|s| s.item.kind == RegistryItemKind::Command)
        .map(|s| s.item)
        .take(config.disclose_commands_max)
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
    // Required tools first, resolved from the FULL survivor set — not just the
    // reranked pool. A tool a selected skill must call can survive the hard
    // filters yet fall outside the top `rerank_pool`; pulling only from `ranked`
    // would then disclose the skill card without its tool. Order the ranked ones
    // by their rank, then any required survivors that ranked outside the pool.
    let rank_of: HashMap<RegistryItemId, usize> = ranked
        .iter()
        .enumerate()
        .map(|(index, scored)| (scored.item.id, index))
        .collect();
    let mut required_tools: Vec<&RegistryItem> = survivors
        .iter()
        .copied()
        .filter(|item| required_set.contains(&item.id))
        .collect();
    required_tools.sort_by_key(|item| rank_of.get(&item.id).copied().unwrap_or(usize::MAX));
    for item in required_tools {
        if disclosed_tool_ids.len() >= config.disclose_tools_max {
            break;
        }
        if tool_set.insert(item.id) {
            disclosed_tool_ids.push(item.id);
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
    let commands: Vec<ToolCard> = disclosed_commands
        .iter()
        .map(|item| ToolCard::of(item))
        .collect();

    let mut selected_ids = disclosed_tool_ids;
    selected_ids.extend(disclosed_skills.iter().map(|item| item.id));
    selected_ids.extend(disclosed_commands.iter().map(|item| item.id));

    Ok(RetrievalResult {
        tools,
        skills,
        commands,
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

/// Keep only the most-specific-scope survivor per `(kind, name)` identity — the
/// registry shadowing contract. Order-preserving and deterministic: on equal
/// specificity the earlier survivor wins, so ranking sees stable input.
fn collapse_shadowed(survivors: Vec<&RegistryItem>) -> Vec<&RegistryItem> {
    let mut winner: HashMap<(RegistryItemKind, String), usize> = HashMap::new();
    let mut kept: Vec<&RegistryItem> = Vec::new();
    for item in survivors {
        let key = (item.kind, item.name.clone());
        match winner.get(&key) {
            Some(&idx) if kept[idx].scope.specificity() >= item.scope.specificity() => {}
            Some(&idx) => kept[idx] = item,
            None => {
                winner.insert(key, kept.len());
                kept.push(item);
            }
        }
    }
    kept
}

/// The four hard filters, in the Chapter 05 order. Any `false` drops the item
/// before it can be scored — a too-risky or out-of-scope item is never merely
/// down-ranked.
fn passes_hard_filters(item: &RegistryItem, query: &RetrievalQuery) -> bool {
    if item.status != RegistryStatus::Active {
        return false;
    }
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
    if item.kind == RegistryItemKind::Skill {
        if let crate::types::Provenance::Package { path } = &item.provenance {
            match std::fs::metadata(path) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => return false,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
                Err(error) => {
                    tracing::warn!(skill = item.name, %error, "registered skill package is unreadable");
                    return false;
                }
            }
        }
    }
    true
}

/// Score every item by exact token overlap with the query and return the
/// `top_k`. A tool whose name/keyword/intent tokens the user typed verbatim is a
/// strong, precise signal — this both seeds the candidate union and feeds the
/// `exact` rerank term.
fn exact_match(items: &[&RegistryItem], query: &str, top_k: usize) -> Vec<(RegistryItemId, f32)> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::{builtin_commands, builtin_tools};

    #[test]
    fn inaccessible_and_shadowed_rows_cannot_consume_candidate_budgets() {
        let repository = codypendent_protocol::RepositoryId::new();
        let mut visible = builtin_tools().remove(0);
        visible.kind = RegistryItemKind::Skill;
        visible.name = "needle".to_string();
        visible.description = "needle".to_string();
        visible.intents = vec!["needle".to_string()];
        visible.keywords = vec!["needle".to_string()];
        visible.scope = Scope::Repository(repository);
        visible.risk = RiskClass::Low;
        let mut rows = Vec::new();
        for reason in 0..5 {
            let mut hidden = visible.clone();
            hidden.id = RegistryItemId::new();
            hidden.scope = Scope::System;
            hidden.description = "needle ".repeat(20);
            match reason {
                0 => hidden.scope = Scope::Repository(codypendent_protocol::RepositoryId::new()),
                1 => hidden.risk = RiskClass::High,
                2 => hidden.trust.tier = TrustTier::Untrusted,
                3 => hidden.status = RegistryStatus::Deprecated,
                _ => {}
            }
            rows.push(hidden);
        }
        rows.push(visible.clone());
        let mut vector = VectorIndex::new();
        for row in &rows {
            vector.insert(
                row.id,
                if row.id == visible.id {
                    vec![0.0, 1.0]
                } else {
                    vec![1.0, 0.0]
                },
            );
        }
        let indexes = RetrievalIndexes::from_parts(&rows, vector, HashingEmbedder::new()).unwrap();
        let mut query = RetrievalQuery::new(
            "needle",
            vec![Scope::Repository(repository)],
            RiskClass::Low,
        );
        query.min_trust = TrustTier::FirstParty;
        query.query_vector = Some(vec![1.0, 0.0]);
        query.history = rows.iter().map(|item| item.id).collect();
        for source in 0..4 {
            let config = RetrievalConfig {
                dense_top: usize::from(source == 0),
                bm25_top: usize::from(source == 1),
                exact_top: usize::from(source == 2),
                history_top: usize::from(source == 3),
                ..RetrievalConfig::default()
            };
            let result = retrieve(&rows, &indexes, &query, &config).unwrap();
            assert_eq!(
                result.trace.candidate_ids,
                vec![visible.id],
                "source {source}"
            );
            assert_eq!(result.skills[0].id, visible.id, "source {source}");
        }
    }

    /// The regression for the silently-discarded third kind: `Command` items pass
    /// the hard filters, enter the candidate union, and get scored — and used to
    /// be dropped before disclosure, so the built-in written for exactly this
    /// sentence could never be surfaced. Asserted with the PRODUCTION registry,
    /// not a fixture, because the bug was that production commands vanished.
    #[test]
    fn a_matching_command_is_disclosed_not_discarded() {
        let items: Vec<RegistryItem> = builtin_tools()
            .into_iter()
            .chain(builtin_commands())
            .collect();
        let indexes = RetrievalIndexes::build(&items, HashingEmbedder::new()).expect("indexes");
        let query = RetrievalQuery::new(
            "the ci is red, fix the failing github check",
            vec![Scope::System],
            RiskClass::High,
        );
        let result = retrieve(&items, &indexes, &query, &RetrievalConfig::default()).expect("ok");

        // The old behaviour, pinned: the command was ALWAYS in the candidate
        // union — retrieved, filtered, scored, and then dropped unpartitioned.
        // If this half ever stops holding, the disclosure assertion below is
        // passing for the wrong reason.
        let fix_ci_id = items
            .iter()
            .find(|item| item.kind == RegistryItemKind::Command && item.name == "fix-ci")
            .map(|item| item.id)
            .expect("`fix-ci` is a built-in command");
        assert!(
            result.trace.candidate_ids.contains(&fix_ci_id),
            "`/fix-ci` was always a candidate; the bug was dropping it before disclosure"
        );

        let disclosed: Vec<&str> = result
            .commands
            .iter()
            .map(|card| card.name.as_str())
            .collect();
        assert!(
            disclosed.contains(&"fix-ci"),
            "`/fix-ci` must reach disclosure for its own intent text: {disclosed:?}"
        );
        // The command budget is separate from the tool budget, so surfacing one
        // never costs a tool card.
        assert!(
            result.tools.len() >= RetrievalConfig::default().disclose_tools_min,
            "commands must not eat into the tool budget: {} tools",
            result.tools.len()
        );
        // Every disclosed id is in the trace, so the audit trail still explains
        // the whole selection.
        for card in &result.commands {
            assert!(
                result.trace.selected_ids.contains(&card.id),
                "a disclosed command must appear in the trace"
            );
        }
    }

    /// `disclose_commands_max: 0` keeps the pre-change behavior exactly, so an
    /// operator (or the runtime's built-in gate, which projects no commands) can
    /// opt out without touching anything else.
    #[test]
    fn a_zero_command_budget_discloses_none() {
        let items: Vec<RegistryItem> = builtin_tools()
            .into_iter()
            .chain(builtin_commands())
            .collect();
        let indexes = RetrievalIndexes::build(&items, HashingEmbedder::new()).expect("indexes");
        let query = RetrievalQuery::new("the ci is red", vec![Scope::System], RiskClass::High);
        let config = RetrievalConfig {
            disclose_commands_max: 0,
            ..RetrievalConfig::default()
        };
        let result = retrieve(&items, &indexes, &query, &config).expect("ok");
        assert!(result.commands.is_empty());
    }
}
