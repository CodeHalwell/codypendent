//! Integration tests for webhook ingestion (Phase 3 STEP 3.3).
//!
//! Covers the security-critical invariants: a forged or unsigned delivery is
//! rejected before any event is produced; a redelivered GUID is idempotent; a
//! policy-off ingestor never marks an event as trigger-worthy; and the raw HTTP
//! listener maps outcomes to the right status codes over a loopback socket.

use std::sync::Arc;

use codypendent_integrations::webhook::ingest::{DeliveryHeaders, IngestOutcome, WebhookIngestor};
use codypendent_integrations::webhook::normalize::NormalizedEvent;
use codypendent_integrations::webhook::server;
use codypendent_integrations::webhook::store::{InMemoryDeliveryStore, SqliteDeliveryStore};
use codypendent_integrations::webhook::verify::sign;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A small, valid `pull_request` payload.
fn pull_request_body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "action": "opened",
        "pull_request": { "number": 7 },
        "repository": { "full_name": "octocat/hello-world" }
    }))
    .expect("serialize fixture")
}

/// Open a migrated SQLite pool under a fresh tempdir.
async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("webhooks.db");
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .connect(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .expect("open pool");
    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .expect("run migrations");
    (dir, pool)
}

#[tokio::test]
async fn forged_signature_rejected() {
    let store = Arc::new(InMemoryDeliveryStore::default());
    let ingestor = WebhookIngestor::new(store, Some(b"correct-secret".to_vec()), false);
    let body = pull_request_body();

    // Signature computed under the WRONG secret.
    let forged = sign(b"wrong-secret", &body);
    let headers = DeliveryHeaders {
        signature: Some(forged),
        event_type: "pull_request".to_string(),
        delivery_id: "guid-forged".to_string(),
    };
    let outcome = ingestor.ingest(&headers, &body).await.expect("ingest");
    assert_eq!(outcome, IngestOutcome::SignatureInvalid);

    // No signature at all when a secret is configured.
    let headers = DeliveryHeaders {
        signature: None,
        event_type: "pull_request".to_string(),
        delivery_id: "guid-unsigned".to_string(),
    };
    let outcome = ingestor.ingest(&headers, &body).await.expect("ingest");
    assert_eq!(outcome, IngestOutcome::SignatureMissing);
    // Neither rejected delivery produced an event.
}

#[tokio::test]
async fn replay_is_idempotent_sqlite() {
    let (_dir, pool) = temp_pool().await;
    let store = Arc::new(SqliteDeliveryStore::new(pool.clone()));
    let ingestor = WebhookIngestor::new(store, None, false);
    let body = pull_request_body();
    let headers = DeliveryHeaders {
        signature: None,
        event_type: "pull_request".to_string(),
        delivery_id: "same-guid".to_string(),
    };

    let first = ingestor.ingest(&headers, &body).await.expect("ingest");
    assert!(matches!(first, IngestOutcome::Accepted { .. }));

    let second = ingestor.ingest(&headers, &body).await.expect("ingest");
    assert_eq!(second, IngestOutcome::Duplicate);

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM webhook_deliveries WHERE delivery_id = ?")
            .bind("same-guid")
            .fetch_one(&pool)
            .await
            .expect("count rows");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn policy_off_no_trigger() {
    let store = Arc::new(InMemoryDeliveryStore::default());
    let ingestor = WebhookIngestor::new(store, None, false);
    let headers = DeliveryHeaders {
        signature: None,
        event_type: "pull_request".to_string(),
        delivery_id: "guid-policy".to_string(),
    };
    let outcome = ingestor
        .ingest(&headers, &pull_request_body())
        .await
        .expect("ingest");
    match outcome {
        IngestOutcome::Accepted { trigger, event } => {
            assert!(!trigger);
            assert_eq!(
                event,
                NormalizedEvent::PullRequest {
                    action: "opened".to_string(),
                    number: 7,
                    repository: "octocat/hello-world".to_string(),
                }
            );
        }
        other => panic!("expected accepted, got {other:?}"),
    }
}

#[tokio::test]
async fn end_to_end_loopback() {
    let secret = b"loopback-secret".to_vec();
    let store = Arc::new(InMemoryDeliveryStore::default());
    let ingestor = Arc::new(WebhookIngestor::new(store, Some(secret.clone()), false));

    let listener = server::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = server::serve(listener, ingestor).await;
    });

    let body = pull_request_body();

    // A valid, signed delivery is accepted (202).
    let signature = sign(&secret, &body);
    let status = send_post(addr, &signature, "guid-valid", &body).await;
    assert_eq!(status, 202);

    // A forged delivery is rejected (401).
    let forged = sign(b"not-the-secret", &body);
    let status = send_post(addr, &forged, "guid-bad", &body).await;
    assert_eq!(status, 401);
}

/// Send a raw HTTP POST and return the numeric status code.
async fn send_post(
    addr: std::net::SocketAddr,
    signature: &str,
    delivery_id: &str,
    body: &[u8],
) -> u16 {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "POST /webhook HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Length: {}\r\n\
         X-Hub-Signature-256: {}\r\n\
         X-GitHub-Event: pull_request\r\n\
         X-GitHub-Delivery: {}\r\n\
         Connection: close\r\n\r\n",
        body.len(),
        signature,
        delivery_id,
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    stream.write_all(body).await.expect("write body");
    stream.flush().await.expect("flush");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    let text = String::from_utf8_lossy(&response);
    // Status line: "HTTP/1.1 <code> <reason>".
    text.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}
