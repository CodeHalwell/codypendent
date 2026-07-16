//! The in-memory dense vector index (Chapter 05, STEP 2.3).
//!
//! Phase 2's vector layer is deliberately the simplest thing that works: a flat
//! `Vec` of `(id, embedding)` searched by brute-force cosine. At registry scale
//! (tens to low thousands of items) this is instant and has no operational
//! surface. A production ANN store (Qdrant) is a Phase-4+ option behind the same
//! call shape, adopted only on measured need — the retrieval funnel only ever
//! asks this index for `search(query, top_k)`, so swapping the backend never
//! touches the funnel.

use codypendent_protocol::RegistryItemId;

/// A brute-force cosine index over item embeddings, rebuildable from
/// [`Registry::list`](crate::registry::Registry::list) at any time (it is a
/// derived index, never authority).
#[derive(Debug, Default, Clone)]
pub struct VectorIndex {
    entries: Vec<(RegistryItemId, Vec<f32>)>,
}

impl VectorIndex {
    /// An empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one item's embedding.
    pub fn insert(&mut self, id: RegistryItemId, embedding: Vec<f32>) {
        self.entries.push((id, embedding));
    }

    /// How many vectors the index holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The `top_k` items most similar to `query` by cosine, highest first.
    ///
    /// Ties break by the item id (via the stable sort on equal scores), so the
    /// result is deterministic for a fixed index.
    #[must_use]
    pub fn search(&self, query: &[f32], top_k: usize) -> Vec<(RegistryItemId, f32)> {
        let mut scored: Vec<(RegistryItemId, f32)> = self
            .entries
            .iter()
            .map(|(id, embedding)| (*id, cosine(query, embedding)))
            .collect();
        // Descending by score; `total_cmp` keeps NaN from poisoning the order.
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(top_k);
        scored
    }
}

/// Cosine similarity of two equal-length vectors; `0.0` if either is empty, a
/// different length, or a zero vector.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut norm_a = 0.0_f32;
    let mut norm_b = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}
