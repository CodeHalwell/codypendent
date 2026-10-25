//! Shared bounded embedding client for the bundled runtime and explicit provider overrides.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use codypendent_knowledge::{EmbedError, SemanticEmbedder};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::models::EmbeddingConfig;

mod bundled;
pub use bundled::BundledEmbedder;

/// Wall-clock bound on ONE `/embeddings` request (the batch is chunked, so this
/// bounds each chunk). Mirrors the fact extractor's 30s model-call budget.
const EMBED_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Largest number of inputs sent in one request. Providers cap batch size
/// (OpenAI at 2048 inputs; local servers far lower), so a registry of any size
/// is chunked rather than refused.
const MAX_BATCH_INPUTS: usize = 64;
const MAX_CACHE_ENTRIES: usize = 4096;

/// The provider string this adapter serves. An `[embedding]` entry naming
/// anything else is rejected at construction rather than silently attempted.
const OPENAI_COMPATIBLE: &str = "openai-compatible";

/// An OpenAI-compatible embedding model, batched and content-hash cached.
///
/// Built with [`HttpEmbedder::from_config`] from the `[embedding]` entry. The
/// API key is resolved from its environment variable **at call time** — never
/// stored on this struct, never logged (Chapter 11, "Secrets") — so a key
/// exported after the daemon started still applies.
pub struct HttpEmbedder {
    client: reqwest::Client,
    /// The fully-resolved endpoint: `{base_url}/embeddings`.
    endpoint: String,
    /// The provider-side model name, sent verbatim and persisted alongside every
    /// vector so a model change invalidates stored rows.
    model: String,
    space_id: String,
    bearer_token: Option<String>,
    batch_size: usize,
    /// The NAME of the environment variable holding the API key (empty for a
    /// local endpoint with no auth). Only the name is retained.
    api_key_env: String,
    /// Expected vector width, when the operator declared one.
    dims: Option<usize>,
    /// hex SHA-256 of an input text → its embedding. Makes a re-embed of an
    /// unchanged registry description (or a repeated query) free within a
    /// process; the `registry_embeddings` table is the cross-process cache.
    cache: Mutex<HashMap<String, Vec<f32>>>,
    requests: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for HttpEmbedder {
    /// Redacting by construction: no key is held, and the cached vectors are
    /// noise in a log — only the endpoint and model identify this embedder.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpEmbedder")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("dims", &self.dims)
            .finish_non_exhaustive()
    }
}

impl HttpEmbedder {
    /// Build an embedder from the `[embedding]` entry.
    ///
    /// Fails (so the caller keeps the hashing embedder) when the provider is not
    /// `openai-compatible`, the base URL is blank, or the model name is blank —
    /// a misconfigured entry is a legible startup warning, never a silently
    /// broken retrieval path.
    pub fn from_config(config: &EmbeddingConfig) -> Result<Self, EmbedError> {
        if config.provider != OPENAI_COMPATIBLE {
            return Err(EmbedError(format!(
                "unsupported embedding provider `{}` (only `{OPENAI_COMPATIBLE}` is wired)",
                config.provider
            )));
        }
        let base = config.base_url.trim().trim_end_matches('/');
        if base.is_empty() {
            return Err(EmbedError("embedding base_url is blank".to_string()));
        }
        if config.model.trim().is_empty() {
            return Err(EmbedError("embedding model is blank".to_string()));
        }
        let mut builder = reqwest::Client::builder().timeout(EMBED_REQUEST_TIMEOUT);
        if reqwest::Url::parse(base).ok().is_some_and(|url| {
            url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|address| address.is_loopback())
            })
        }) {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(|error| {
            EmbedError(format!("could not build the embedding client: {error}"))
        })?;
        Ok(Self {
            client,
            endpoint: format!("{base}/embeddings"),
            model: config.model.trim().to_string(),
            space_id: format!(
                "{base}/{}:dims={:?}:retrieval-v1",
                config.model.trim(),
                config.dims
            ),
            bearer_token: None,
            batch_size: MAX_BATCH_INPUTS,
            api_key_env: config.api_key_env.trim().to_string(),
            dims: config.dims,
            cache: Mutex::new(HashMap::new()),
            requests: tokio::sync::Mutex::new(()),
        })
    }

    pub(crate) fn with_identity(mut self, space_id: String, token: String) -> Self {
        self.space_id = space_id;
        self.bearer_token = Some(token);
        self.batch_size = 16;
        self
    }

    /// The endpoint this embedder posts to (diagnostics + tests).
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Read the API key from its configured environment variable, at call time.
    /// An unset or blank variable means "no auth" (a local endpoint), matching
    /// [`ModelConfig::api_key_env`](crate::models::ModelConfig::api_key_env).
    fn api_key(&self) -> Option<String> {
        if let Some(token) = &self.bearer_token {
            return Some(token.clone());
        }
        if self.api_key_env.is_empty() {
            return None;
        }
        std::env::var(&self.api_key_env)
            .ok()
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
    }

    /// POST one chunk of inputs and return their vectors **in input order**.
    async fn embed_chunk(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut request = self
            .client
            .post(&self.endpoint)
            .json(&json!({"model": self.model, "input": inputs}));
        if let Some(key) = self.api_key() {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|error| EmbedError(format!("{} unreachable: {error}", self.endpoint)))?;
        let status = response.status();
        if !status.is_success() {
            // The body of an error response can echo request content; report the
            // status only.
            return Err(EmbedError(format!(
                "{} returned HTTP {status}",
                self.endpoint
            )));
        }
        let payload: serde_json::Value = response
            .json()
            .await
            .map_err(|error| EmbedError(format!("invalid embeddings response: {error}")))?;
        parse_embeddings(&payload, inputs.len(), self.dims)
    }
}

/// Decode an OpenAI-shaped embeddings response into `expected` vectors, ordered
/// by each entry's `index` (the spec does not promise request order), and verify
/// they share one width — matching `dims` when the operator declared one.
///
/// A missing/duplicate index, a short array, a non-numeric component, or a width
/// disagreement is an error rather than a silently degraded vector: retrieval
/// ranking a query against half-decoded vectors would be worse than falling back
/// to the deterministic hashing model.
fn parse_embeddings(
    payload: &serde_json::Value,
    expected: usize,
    dims: Option<usize>,
) -> Result<Vec<Vec<f32>>, EmbedError> {
    let data = payload
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| EmbedError("embeddings response has no `data` array".to_string()))?;
    if data.len() != expected {
        return Err(EmbedError(format!(
            "embeddings response carried {} vectors for {expected} inputs",
            data.len()
        )));
    }

    let mut ordered: Vec<Option<Vec<f32>>> = vec![None; expected];
    for (position, entry) in data.iter().enumerate() {
        // `index` is optional in practice (some local servers omit it); fall
        // back to the entry's position, which is the order every server that
        // omits it uses.
        let index = entry
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .map_or(position, |i| i as usize);
        let slot = ordered
            .get_mut(index)
            .ok_or_else(|| EmbedError(format!("embeddings response index {index} out of range")))?;
        if slot.is_some() {
            return Err(EmbedError(format!(
                "embeddings response repeated index {index}"
            )));
        }
        let vector = entry
            .get("embedding")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| EmbedError(format!("embeddings entry {index} has no `embedding`")))?
            .iter()
            .map(|value| {
                value
                    .as_f64()
                    .map(|v| v as f32)
                    .ok_or_else(|| EmbedError(format!("embeddings entry {index} is not numeric")))
            })
            .collect::<Result<Vec<f32>, EmbedError>>()?;
        if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
            return Err(EmbedError(format!("embeddings entry {index} is empty")));
        }
        *slot = Some(vector);
    }

    let vectors: Vec<Vec<f32>> = ordered
        .into_iter()
        .enumerate()
        .map(|(index, slot)| {
            slot.ok_or_else(|| EmbedError(format!("embeddings response skipped index {index}")))
        })
        .collect::<Result<_, EmbedError>>()?;

    // One width across the whole batch, and the declared one when configured —
    // a mismatch means this endpoint is not the model the persisted vectors were
    // written under (knowledge's freshness check catches the persisted half).
    let width = vectors[0].len();
    if let Some(expected_dims) = dims {
        if width != expected_dims {
            return Err(EmbedError(format!(
                "embedding model returned {width}-dimensional vectors, but `dims` declares {expected_dims}"
            )));
        }
    }
    if let Some(odd) = vectors.iter().position(|vector| vector.len() != width) {
        return Err(EmbedError(format!(
            "embeddings response mixed widths ({width} and {})",
            vectors[odd].len()
        )));
    }
    Ok(vectors)
}

/// The hex SHA-256 cache key for one input text.
fn cache_key(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

#[async_trait::async_trait]
impl SemanticEmbedder for HttpEmbedder {
    fn model(&self) -> &str {
        &self.model
    }

    fn space_id(&self) -> &str {
        &self.space_id
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let input = if self.model.contains("embeddinggemma") {
            format!("task: search result | query: {text}")
        } else {
            text.to_string()
        };
        let mut vectors = self.embed_cached(&[input]).await?;
        vectors
            .pop()
            .ok_or_else(|| EmbedError("query embedding missing".to_string()))
    }

    /// Embed `texts` in input order, serving what the content-hash cache already
    /// holds and requesting only the rest — chunked into [`MAX_BATCH_INPUTS`]
    /// requests. A repeated text within one batch is requested once.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let texts = texts
            .iter()
            .map(|text| {
                if self.model.contains("embeddinggemma") {
                    format!("title: none | text: {text}")
                } else {
                    text.clone()
                }
            })
            .collect::<Vec<_>>();
        self.embed_cached(&texts).await
    }
}

impl HttpEmbedder {
    async fn embed_cached(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let keys: Vec<String> = texts.iter().map(|text| cache_key(text)).collect();
        let active = keys.iter().collect::<HashSet<_>>();
        if active.len() > MAX_CACHE_ENTRIES {
            return Err(EmbedError(
                "embedding request exceeds the bounded cache capacity".to_string(),
            ));
        }
        if let Some(vectors) = self.cached(&keys) {
            return Ok(vectors);
        }

        // Split into cache hits and the distinct texts still to request.
        let mut pending: Vec<String> = Vec::new();
        let mut pending_keys: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        {
            let cache = self.cache.lock().expect("embedding cache poisoned");
            for (text, key) in texts.iter().zip(&keys) {
                if !cache.contains_key(key) && seen.insert(key) {
                    pending.push(text.clone());
                    pending_keys.push(key.clone());
                }
            }
        }

        // Request the misses, chunked; fill the cache as each chunk lands so a
        // later failure still leaves the earlier work cached.
        for (chunk, chunk_keys) in pending
            .chunks(self.batch_size)
            .zip(pending_keys.chunks(self.batch_size))
        {
            let _request = self.requests.lock().await;
            let missing = {
                let cache = self.cache.lock().expect("embedding cache poisoned");
                chunk
                    .iter()
                    .zip(chunk_keys)
                    .filter(|(_, key)| !cache.contains_key(*key))
                    .map(|(text, key)| (text.clone(), key.clone()))
                    .collect::<Vec<_>>()
            };
            let inputs = missing
                .iter()
                .map(|(text, _)| text.clone())
                .collect::<Vec<_>>();
            let vectors = if inputs.is_empty() {
                Vec::new()
            } else {
                self.embed_chunk(&inputs).await?
            };
            let mut cache = self.cache.lock().expect("embedding cache poisoned");
            for ((_, key), vector) in missing.iter().zip(vectors) {
                if cache.len() >= MAX_CACHE_ENTRIES && !cache.contains_key(key) {
                    cache.retain(|key, _| active.contains(key));
                }
                cache.insert(key.clone(), vector);
            }
        }

        let cache = self.cache.lock().expect("embedding cache poisoned");
        let result = keys
            .iter()
            .map(|key| {
                cache
                    .get(key)
                    .cloned()
                    .ok_or_else(|| EmbedError("embedding cache miss after request".to_string()))
            })
            .collect();
        result
    }

    fn cached(&self, keys: &[String]) -> Option<Vec<Vec<f32>>> {
        let cache = self.cache.lock().expect("embedding cache poisoned");
        keys.iter().map(|key| cache.get(key).cloned()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn cached_queries_do_not_wait_for_indexing() {
        let embedder = HttpEmbedder::from_config(&config("http://127.0.0.1:1/v1", "m")).unwrap();
        embedder
            .cache
            .lock()
            .unwrap()
            .insert(cache_key("hot"), vec![1.0, 2.0]);
        let _indexing = embedder.requests.lock().await;
        let vectors = tokio::time::timeout(
            Duration::from_millis(100),
            embedder.embed_batch(&["hot".to_string()]),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(vectors, vec![vec![1.0, 2.0]]);
    }

    #[tokio::test]
    async fn new_queries_run_between_indexing_batches() {
        use std::sync::Arc;
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(|request: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let data = body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .map(|(index, _)| json!({"index":index,"embedding":[1.0,2.0]}))
                    .collect::<Vec<_>>();
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(50))
                    .set_body_json(json!({"data":data}))
            })
            .expect(3)
            .mount(&server)
            .await;
        let mut embedder = HttpEmbedder::from_config(&config(&server.uri(), "m")).unwrap();
        embedder.batch_size = 2;
        let embedder = Arc::new(embedder);
        let indexing = embedder.clone();
        let background = tokio::spawn(async move {
            indexing
                .embed_batch(&["a".into(), "b".into(), "c".into(), "d".into()])
                .await
                .unwrap()
        });
        while server.received_requests().await.unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        embedder.embed_query("new query").await.unwrap();
        background.await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let middle: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(middle["input"][0], "new query");
    }

    #[tokio::test]
    async fn partial_failures_cannot_expand_the_cache() {
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(|request: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let inputs = body["input"].as_array().unwrap();
                if inputs[0] == "64" {
                    return ResponseTemplate::new(503);
                }
                let data = inputs
                    .iter()
                    .enumerate()
                    .map(|(index, _)| json!({"index":index,"embedding":[1.0,2.0]}))
                    .collect::<Vec<_>>();
                ResponseTemplate::new(200).set_body_json(json!({"data":data}))
            })
            .expect(2)
            .mount(&server)
            .await;
        let embedder = HttpEmbedder::from_config(&config(&server.uri(), "m")).unwrap();
        embedder
            .cache
            .lock()
            .unwrap()
            .extend((0..MAX_CACHE_ENTRIES).map(|n| (format!("old-{n}"), vec![1.0, 2.0])));
        assert!(embedder
            .embed_batch(&(0..65).map(|n| n.to_string()).collect::<Vec<_>>())
            .await
            .is_err());
        assert!(embedder.cache.lock().unwrap().len() <= MAX_CACHE_ENTRIES);
    }

    fn config(base_url: &str, model: &str) -> EmbeddingConfig {
        EmbeddingConfig {
            provider: OPENAI_COMPATIBLE.to_string(),
            base_url: base_url.to_string(),
            model: model.to_string(),
            api_key_env: String::new(),
            dims: None,
        }
    }

    #[test]
    fn from_config_rejects_an_unwired_provider_or_blank_fields() {
        let mut unwired = config("http://localhost:11434/v1", "nomic-embed-text");
        unwired.provider = "cohere".to_string();
        assert!(HttpEmbedder::from_config(&unwired).is_err());

        assert!(HttpEmbedder::from_config(&config("", "nomic-embed-text")).is_err());
        assert!(HttpEmbedder::from_config(&config("http://x/v1", "  ")).is_err());
    }

    /// The endpoint is the base URL plus `/embeddings`, with a trailing slash on
    /// the base absorbed (so `http://host/v1/` and `http://host/v1` agree).
    #[test]
    fn endpoint_appends_embeddings_to_the_base_url() {
        let embedder =
            HttpEmbedder::from_config(&config("http://localhost:11434/v1/", "nomic-embed-text"))
                .expect("builds");
        assert_eq!(embedder.endpoint(), "http://localhost:11434/v1/embeddings");
        assert_eq!(embedder.model(), "nomic-embed-text");
    }

    // -- parse_embeddings ---------------------------------------------------

    #[test]
    fn parse_orders_by_index_not_response_order() {
        let payload = json!({"data": [
            {"index": 1, "embedding": [0.0, 1.0]},
            {"index": 0, "embedding": [1.0, 0.0]},
        ]});
        let vectors = parse_embeddings(&payload, 2, None).expect("parses");
        assert_eq!(vectors, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
    }

    #[test]
    fn parse_falls_back_to_position_when_index_is_absent() {
        let payload = json!({"data": [
            {"embedding": [1.0, 0.0]},
            {"embedding": [0.0, 1.0]},
        ]});
        let vectors = parse_embeddings(&payload, 2, None).expect("parses");
        assert_eq!(vectors, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
    }

    #[test]
    fn parse_rejects_a_declared_dims_mismatch() {
        let payload = json!({"data": [{"index": 0, "embedding": [1.0, 0.0, 0.0]}]});
        assert!(parse_embeddings(&payload, 1, Some(3)).is_ok());
        let error = parse_embeddings(&payload, 1, Some(768)).expect_err("dims mismatch");
        assert!(error.to_string().contains("768"), "{error}");
    }

    #[test]
    fn parse_rejects_malformed_bodies() {
        // No `data` array at all.
        assert!(parse_embeddings(&json!({"error": "nope"}), 1, None).is_err());
        // Fewer vectors than inputs.
        assert!(parse_embeddings(&json!({"data": []}), 1, None).is_err());
        // A repeated index leaves another slot unfilled.
        let repeated = json!({"data": [
            {"index": 0, "embedding": [1.0]},
            {"index": 0, "embedding": [1.0]},
        ]});
        assert!(parse_embeddings(&repeated, 2, None).is_err());
        // An out-of-range index.
        let out_of_range = json!({"data": [{"index": 7, "embedding": [1.0]}]});
        assert!(parse_embeddings(&out_of_range, 1, None).is_err());
        // A non-numeric component.
        let ragged = json!({"data": [{"index": 0, "embedding": ["x"]}]});
        assert!(parse_embeddings(&ragged, 1, None).is_err());
        // Mixed widths across the batch.
        let mixed = json!({"data": [
            {"index": 0, "embedding": [1.0, 0.0]},
            {"index": 1, "embedding": [1.0]},
        ]});
        assert!(parse_embeddings(&mixed, 2, None).is_err());
    }
}
