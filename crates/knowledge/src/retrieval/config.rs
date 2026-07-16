//! The versioned retrieval configuration (Chapter 05, STEP 2.3).
//!
//! Every knob the funnel turns — candidate sizes, the rerank weights, and the
//! progressive-disclosure budget — lives in one struct with a `version`. Phase 7
//! learning will tune these values against the evaluation set; because the
//! version is stamped into every [`RetrievalTrace`](super::RetrievalTrace), a
//! trace always records *which* configuration produced its selection, so a later
//! tuning run is never confused for the behaviour that generated an old trace.

use serde::{Deserialize, Serialize};

/// The weighted-sum rerank coefficients (Chapter 05 "Scoring"). Each is applied
/// to a signal normalized to roughly `[0, 1]`; the two penalties are subtracted.
///
/// Security is **not** among these weights: forbidden items are removed by a hard
/// filter before rerank ever runs (`risk_penalty` only *orders* the survivors, it
/// never lets a too-risky item through).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RerankWeights {
    /// Dense (embedding cosine) relevance.
    pub dense: f32,
    /// Lexical (BM25) relevance.
    pub lexical: f32,
    /// Exact identifier / keyword / intent-token overlap.
    pub exact: f32,
    /// Dependency relevance — a tool required by a query-relevant skill.
    pub dependency: f32,
    /// Subtracted: coarse [`RiskClass`](crate::types::RiskClass) as a ranking
    /// nudge (the hard risk-ceiling filter, not this, is what excludes danger).
    pub risk_penalty: f32,
    /// Added: provenance [`TrustTier`](crate::types::TrustTier) — first-party
    /// items are surfaced ahead of untrusted ones of equal relevance.
    pub trust_bonus: f32,
}

impl Default for RerankWeights {
    fn default() -> Self {
        Self {
            dense: 1.0,
            lexical: 1.0,
            // Exact identifier/keyword hits are the strongest disambiguator
            // between the real tools and lexically-adjacent decoys, so they are
            // weighted above the fuzzy signals.
            exact: 2.0,
            dependency: 0.5,
            risk_penalty: 0.25,
            trust_bonus: 0.25,
        }
    }
}

/// The full, versioned retrieval configuration. Candidate sizes follow the
/// Chapter 05 suggested initial values; the disclosure counts are the
/// context-budget bounds (6–12 tool cards, 1–3 skill cards).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RetrievalConfig {
    /// Bumped whenever any value below changes; recorded in every trace.
    pub version: u32,
    /// Dense candidate cut (Chapter 05 suggests 100).
    pub dense_top: usize,
    /// BM25 candidate cut (Chapter 05 suggests 100).
    pub bm25_top: usize,
    /// Exact id/keyword/intent candidate cut (Chapter 05 suggests 50).
    pub exact_top: usize,
    /// History candidate cut (Chapter 05 suggests 50).
    pub history_top: usize,
    /// How many survivors to keep after rerank before disclosure (30–50).
    pub rerank_pool: usize,
    /// Minimum tool cards to disclose when that many survive.
    pub disclose_tools_min: usize,
    /// Maximum tool cards to disclose (the hard context budget).
    pub disclose_tools_max: usize,
    /// Minimum skill cards to disclose when that many survive.
    pub disclose_skills_min: usize,
    /// Maximum skill cards to disclose.
    pub disclose_skills_max: usize,
    /// The rerank weights.
    pub weights: RerankWeights,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            version: 1,
            dense_top: 100,
            bm25_top: 100,
            exact_top: 50,
            history_top: 50,
            rerank_pool: 50,
            disclose_tools_min: 6,
            disclose_tools_max: 12,
            disclose_skills_min: 1,
            disclose_skills_max: 3,
            weights: RerankWeights::default(),
        }
    }
}
