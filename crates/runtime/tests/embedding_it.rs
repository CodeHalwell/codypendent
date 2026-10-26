//! The HTTP embedder against a mock OpenAI-compatible `/embeddings` endpoint
//! (rubric 9): the wire shape Ollama, OpenAI, and Nebius all serve, batching,
//! the content-hash cache, auth, and the failure modes that must degrade
//! retrieval to the hashing embedder rather than fail a run.

use codypendent_knowledge::SemanticEmbedder;
use codypendent_runtime::embedding::HttpEmbedder;
use codypendent_runtime::models::EmbeddingConfig;
use serde_json::{json, Value};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// An `[embedding]` entry pointed at `base_url`.
fn config(base_url: &str, model: &str) -> EmbeddingConfig {
    EmbeddingConfig {
        provider: "openai-compatible".to_string(),
        base_url: base_url.to_string(),
        model: model.to_string(),
        api_key_env: String::new(),
        dims: None,
    }
}

/// Respond to a request by embedding each input into a deterministic 3-vector,
/// echoing the OpenAI response shape (with `index`, deliberately reversed so an
/// implementation that trusts response order fails).
fn responder(request: &Request) -> ResponseTemplate {
    let body: Value = serde_json::from_slice(&request.body).expect("json body");
    let inputs = body["input"].as_array().expect("input array");
    let mut data: Vec<Value> = inputs
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let text = value.as_str().unwrap_or_default().to_ascii_lowercase();
            json!({
                "index": index,
                "object": "embedding",
                "embedding": [
                    text.matches("test").count() as f64,
                    text.matches("diff").count() as f64,
                    text.len() as f64 / 100.0,
                ],
            })
        })
        .collect();
    data.reverse();
    ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": data}))
}

/// The happy path against the shape every OpenAI-compatible server speaks:
/// `POST {base}/embeddings` with `{model, input: [...]}`, answered with a `data`
/// array the embedder re-orders by `index`.
#[tokio::test]
async fn embeds_a_batch_against_an_openai_compatible_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(responder)
        .expect(1)
        .mount(&server)
        .await;

    let embedder =
        HttpEmbedder::from_config(&config(&format!("{}/v1", server.uri()), "nomic-embed-text"))
            .expect("builds");
    assert_eq!(embedder.model(), "nomic-embed-text");

    let texts = vec![
        "run the tests".to_string(),
        "show the diff diff".to_string(),
    ];
    let vectors = embedder.embed_batch(&texts).await.expect("embeds");
    assert_eq!(vectors.len(), 2);
    // Input ORDER is preserved even though the server answered index-reversed.
    assert_eq!(vectors[0][0], 1.0, "first input is the tests one");
    assert_eq!(vectors[1][1], 2.0, "second input is the diff one");

    let sent: Value = serde_json::from_slice(&server.received_requests().await.unwrap()[0].body)
        .expect("json body");
    assert_eq!(sent["model"], "nomic-embed-text");
    assert_eq!(
        sent["input"],
        json!(["run the tests", "show the diff diff"])
    );
}

/// The content-hash cache: re-embedding the same text never re-requests it, and
/// a repeated text within one batch is requested once. `expect(1)` on the mock
/// is the assertion — a second request fails the test at drop.
#[tokio::test]
async fn caches_by_content_hash_across_and_within_batches() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(responder)
        .expect(1)
        .mount(&server)
        .await;

    let embedder =
        HttpEmbedder::from_config(&config(&format!("{}/v1", server.uri()), "nomic-embed-text"))
            .expect("builds");

    // One request covering both distinct texts, with a duplicate riding along.
    let first = embedder
        .embed_batch(&["alpha".to_string(), "beta".to_string(), "alpha".to_string()])
        .await
        .expect("embeds");
    assert_eq!(first.len(), 3);
    assert_eq!(first[0], first[2], "a repeat resolves to the same vector");

    // Fully cached: no second request reaches the server.
    let second = embedder
        .embed_batch(&["beta".to_string(), "alpha".to_string()])
        .await
        .expect("embeds from cache");
    assert_eq!(second[0], first[1]);
    assert_eq!(second[1], first[0]);
}

/// A configured `api_key_env` is read at CALL time and sent as a bearer token;
/// the key itself is never stored on the embedder.
#[tokio::test]
async fn sends_the_api_key_from_its_environment_variable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .and(header("authorization", "Bearer nebius-secret"))
        .respond_with(responder)
        .expect(1)
        .mount(&server)
        .await;

    let mut cfg = config(&format!("{}/v1", server.uri()), "Qwen/Qwen3-Embedding-8B");
    cfg.api_key_env = "CODYPENDENT_TEST_EMBEDDING_KEY".to_string();
    // The variable name is unique to this test, so no other test races it (the
    // `models.rs` env-var tests use the same convention).
    std::env::set_var("CODYPENDENT_TEST_EMBEDDING_KEY", "nebius-secret");

    let embedder = HttpEmbedder::from_config(&cfg).expect("builds");
    let vectors = embedder
        .embed_batch(&["hosted model input".to_string()])
        .await
        .expect("embeds");
    assert_eq!(vectors.len(), 1);
    assert!(
        format!("{embedder:?}").contains("Qwen/Qwen3-Embedding-8B"),
        "Debug names the model"
    );
    assert!(
        !format!("{embedder:?}").contains("nebius-secret"),
        "Debug must never carry the key"
    );

    std::env::remove_var("CODYPENDENT_TEST_EMBEDDING_KEY");
}

/// A declared `dims` that the endpoint contradicts is an error, not a silently
/// wrong-width vector — the guard against pointing `[embedding]` at the wrong
/// model after vectors were persisted under another.
#[tokio::test]
async fn a_dims_mismatch_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(responder)
        .mount(&server)
        .await;

    let mut cfg = config(&format!("{}/v1", server.uri()), "nomic-embed-text");
    cfg.dims = Some(768); // the responder returns 3-vectors
    let embedder = HttpEmbedder::from_config(&cfg).expect("builds");
    let error = embedder
        .embed_batch(&["anything".to_string()])
        .await
        .expect_err("dims mismatch");
    assert!(error.to_string().contains("768"), "{error}");
}

/// An endpoint that errors (or answers with junk) yields `EmbedError` rather
/// than a panic — the input knowledge's `semantic_indexes` degrades on.
#[tokio::test]
async fn an_http_error_or_junk_body_yields_a_legible_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/broken/embeddings"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/junk/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;

    let broken = HttpEmbedder::from_config(&config(&format!("{}/broken", server.uri()), "m"))
        .expect("built");
    let error = broken
        .embed_batch(&["x".to_string()])
        .await
        .expect_err("HTTP 503");
    assert!(error.to_string().contains("503"), "{error}");

    let junk =
        HttpEmbedder::from_config(&config(&format!("{}/junk", server.uri()), "m")).expect("built");
    assert!(junk.embed_batch(&["x".to_string()]).await.is_err());

    // An unreachable host fails the same way (no panic, no hang past the bound).
    let dead = HttpEmbedder::from_config(&config("http://127.0.0.1:1/v1", "m")).expect("built");
    assert!(dead.embed_batch(&["x".to_string()]).await.is_err());
}

/// An empty batch never touches the network — `semantic_indexes` calls this
/// path whenever every item's vector was already persisted.
#[tokio::test]
async fn an_empty_batch_makes_no_request() {
    let embedder =
        HttpEmbedder::from_config(&config("http://127.0.0.1:1/v1", "m")).expect("builds");
    assert!(embedder.embed_batch(&[]).await.expect("no-op").is_empty());
}

#[tokio::test]
async fn gemma_uses_separate_query_and_document_tasks() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(responder)
        .expect(2)
        .mount(&server)
        .await;
    let embedder =
        HttpEmbedder::from_config(&config(&format!("{}/v1", server.uri()), "embeddinggemma-2"))
            .unwrap();
    embedder
        .embed_batch(&["restore a checkpoint".to_string()])
        .await
        .unwrap();
    embedder.embed_query("undo my work").await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let document: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let query: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(
        document["input"][0],
        "title: none | text: restore a checkpoint"
    );
    assert_eq!(
        query["input"][0],
        "task: search result | query: undo my work"
    );
    assert!(embedder.space_id().contains("retrieval-v1"));
}

#[tokio::test]
#[ignore = "runs the packaged EmbeddingGemma 2 weights"]
async fn bundled_gemma_retrieves_a_paraphrase_offline() {
    use codypendent_runtime::embedding::BundledEmbedder;
    use std::time::{Duration, Instant};
    let model = BundledEmbedder::discover().expect("prepared bundle");
    let started = Instant::now();
    let query = loop {
        if let Ok(vector) = model
            .embed_query("undo my latest changes and go back to an earlier saved state")
            .await
        {
            break vector;
        }
        assert!(
            started.elapsed() < Duration::from_secs(50),
            "bundled model never became ready"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let documents = model
        .embed_batch(&[
            "Restore a previous checkpoint of the repository and its files".to_string(),
            "Search the internet for current weather forecasts".to_string(),
            "Prepare an invoice and charge the customer's credit card".to_string(),
        ])
        .await
        .unwrap();
    let cosine = |a: &[f32], b: &[f32]| {
        a.iter().zip(b).map(|(a, b)| a * b).sum::<f32>()
            / (a.iter().map(|v| v * v).sum::<f32>().sqrt()
                * b.iter().map(|v| v * v).sum::<f32>().sqrt())
    };
    let scores: Vec<_> = documents.iter().map(|v| cosine(&query, v)).collect();
    assert_eq!(query.len(), 768);
    assert!(scores[0] > scores[1] && scores[0] > scores[2], "{scores:?}");
    let warm = Instant::now();
    assert_eq!(
        query,
        model
            .embed_query("undo my latest changes and go back to an earlier saved state")
            .await
            .unwrap()
    );
    println!(
        "bundled_model={} space={} ready_ms={} scores={scores:?} cached_query_us={}",
        model.model(),
        model.space_id(),
        started.elapsed().as_millis(),
        warm.elapsed().as_micros()
    );
}
