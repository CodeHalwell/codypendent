//! Offline dense embeddings for retrieval (Chapter 05, STEP 2.3).
//!
//! Dense retrieval is kept behind a trait so the vector layer stays abstract
//! (the [manual-index](../../../docs/docs/00-index.md) stance): Phase 2 ships a
//! small, deterministic, dependency-free [`HashingEmbedder`], and a real
//! embedding model — configured via `models.toml`'s `embedding` entry — plugs in
//! behind the same [`Embedder`] trait later without touching the funnel. Because
//! the hashing embedder is pure and deterministic, the evaluation gate is
//! reproducible offline with no model download.

use std::collections::HashMap;
use std::sync::Mutex;

use sha2::{Digest, Sha256};

/// The fixed dimensionality of the [`HashingEmbedder`] space. A real model plugs
/// in behind [`Embedder`] with its own (larger) dimension.
pub const EMBEDDING_DIMENSION: usize = 512;

/// Turn text into a dense vector for cosine retrieval.
///
/// Implementations should return vectors of a consistent dimension; the
/// [`VectorIndex`](super::VectorIndex) compares them by cosine, so returning
/// L2-normalized vectors is conventional but not required (the index normalizes
/// defensively).
pub trait Embedder {
    /// Embed `text` into a dense vector.
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// A deterministic, offline embedder: hashed character trigrams, term-frequency
/// weighted, projected into [`EMBEDDING_DIMENSION`] buckets and L2-normalized.
///
/// Character trigrams give the vector a graceful, sub-word notion of similarity
/// ("changed" and "changes" share the trigrams `cha`, `han`, `ang`, `nge`), so
/// dense retrieval still fires when exact tokenization would miss a morphological
/// variant. It is not a semantic model — it is the Phase-2 stand-in the doc calls
/// a "small embedded implementation" — but it is fully reproducible.
///
/// Embeddings are cached by the SHA-256 of their input text, so re-embedding the
/// same registry description (or a repeated query) is free.
#[derive(Debug, Default)]
pub struct HashingEmbedder {
    /// content-hash (hex SHA-256 of the text) → embedding.
    cache: Mutex<HashMap<String, Vec<f32>>>,
}

impl HashingEmbedder {
    /// A fresh embedder with an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Compute the raw embedding (no caching) — the deterministic core.
    fn compute(text: &str) -> Vec<f32> {
        let mut vector = vec![0.0_f32; EMBEDDING_DIMENSION];

        // Normalize to lowercase alphanumerics separated by single spaces, then
        // frame with boundary spaces so word-initial/-final trigrams are distinct
        // (" ru", "un " for "run"), which sharpens short-token similarity.
        let normalized: String = text
            .chars()
            .map(|c| {
                if c.is_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    ' '
                }
            })
            .collect();
        let framed: Vec<char> = format!(
            " {} ",
            normalized.split_whitespace().collect::<Vec<_>>().join(" ")
        )
        .chars()
        .collect();

        // Term-frequency over hashed trigrams.
        for window in framed.windows(3) {
            if window.iter().all(|c| *c == ' ') {
                continue;
            }
            let bucket = trigram_bucket(window);
            vector[bucket] += 1.0;
        }

        l2_normalize(&mut vector);
        vector
    }
}

impl Embedder for HashingEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        let key = hex::encode(Sha256::digest(text.as_bytes()));
        if let Some(cached) = self
            .cache
            .lock()
            .expect("embedding cache poisoned")
            .get(&key)
        {
            return cached.clone();
        }
        let vector = Self::compute(text);
        self.cache
            .lock()
            .expect("embedding cache poisoned")
            .insert(key, vector.clone());
        vector
    }
}

/// Hash one character trigram into a bucket index via FNV-1a over its UTF-8
/// bytes — small, deterministic, and platform-independent.
fn trigram_bucket(trigram: &[char]) -> usize {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    let mut buf = [0_u8; 4];
    for ch in trigram {
        for byte in ch.encode_utf8(&mut buf).bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    (hash % EMBEDDING_DIMENSION as u64) as usize
}

/// Scale a vector to unit L2 norm in place (a zero vector is left as zeros).
fn l2_normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in vector.iter_mut() {
            *value /= norm;
        }
    }
}
