//! Ingestion orchestration: verify → dedup → normalize.
//!
//! [`WebhookIngestor::ingest`] enforces the ordering that makes ingestion safe:
//! signature verification happens **before** the body is parsed, replay
//! deduplication happens **before** any event is produced, and normalization
//! only runs for a fresh, authenticated delivery. Workflows are triggered solely
//! when policy explicitly permits it (`allow_triggers`, default `false`).

use std::sync::Arc;

use super::store::DeliveryStore;
use super::{normalize, verify, WebhookError};

/// Orchestrates verification, replay dedup, and normalization for one endpoint.
pub struct WebhookIngestor {
    store: Arc<dyn DeliveryStore>,
    /// The HMAC secret, if signature verification is enabled. When `None`, the
    /// signature step is skipped (useful in trusted/local setups and tests).
    secret: Option<Vec<u8>>,
    /// Whether accepted events may trigger workflows. Default `false`.
    allow_triggers: bool,
}

/// The outcome of ingesting one delivery.
#[derive(Debug, PartialEq)]
pub enum IngestOutcome {
    /// A secret is configured but the request carried no signature.
    SignatureMissing,
    /// The signature was present but did not verify.
    SignatureInvalid,
    /// The delivery GUID was already recorded; no event is produced.
    Duplicate,
    /// A fresh, authenticated delivery normalized into an event.
    Accepted {
        event: normalize::NormalizedEvent,
        /// Whether this event may trigger a workflow, per policy.
        trigger: bool,
    },
}

/// The GitHub headers ingestion depends on, extracted from the request.
pub struct DeliveryHeaders {
    /// The `X-Hub-Signature-256` value (`sha256=<hex>`), if present.
    pub signature: Option<String>,
    /// The `X-GitHub-Event` value.
    pub event_type: String,
    /// The `X-GitHub-Delivery` GUID.
    pub delivery_id: String,
}

impl WebhookIngestor {
    /// Build an ingestor over `store`. Pass `Some(secret)` to require signature
    /// verification; `allow_triggers` gates whether accepted events may trigger
    /// workflows.
    pub fn new(
        store: Arc<dyn DeliveryStore>,
        secret: Option<Vec<u8>>,
        allow_triggers: bool,
    ) -> Self {
        Self {
            store,
            secret,
            allow_triggers,
        }
    }

    /// Whether this ingestor is permitted to trigger workflows.
    pub fn should_trigger(&self) -> bool {
        self.allow_triggers
    }

    /// Ingest one delivery.
    ///
    /// Verification runs before the body is parsed; deduplication runs before
    /// any event is produced.
    pub async fn ingest(
        &self,
        headers: &DeliveryHeaders,
        body: &[u8],
    ) -> Result<IngestOutcome, WebhookError> {
        // 0. A delivery id is the dedup key: an empty one must never reach the
        // store, where it would be recorded once and then mark every later
        // id-less delivery a `Duplicate`. The HTTP listener also rejects this,
        // but the invariant belongs to the reusable ingestor, not one transport.
        if headers.delivery_id.is_empty() || headers.event_type.is_empty() {
            return Err(WebhookError::Malformed(
                "missing X-GitHub-Delivery / X-GitHub-Event".to_string(),
            ));
        }

        // 1. Verify the signature over the raw bytes, before any parsing.
        if let Some(secret) = &self.secret {
            let Some(signature) = &headers.signature else {
                return Ok(IngestOutcome::SignatureMissing);
            };
            if !verify::verify_signature(secret, body, signature) {
                return Ok(IngestOutcome::SignatureInvalid);
            }
        }

        // 2. Deduplicate by delivery GUID, before producing any event.
        let is_new = self
            .store
            .record_if_new(&headers.delivery_id, &headers.event_type)
            .await?;
        if !is_new {
            return Ok(IngestOutcome::Duplicate);
        }

        // 3. Normalize the now-trusted, first-seen delivery.
        let event = normalize::normalize(&headers.event_type, body)?;
        Ok(IngestOutcome::Accepted {
            event,
            trigger: self.allow_triggers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webhook::store::InMemoryDeliveryStore;
    use crate::webhook::verify::sign;

    fn pull_request_body() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "action": "opened",
            "pull_request": { "number": 7 },
            "repository": { "full_name": "octocat/hello-world" }
        }))
        .expect("serialize fixture")
    }

    fn headers(delivery_id: &str, signature: Option<String>) -> DeliveryHeaders {
        DeliveryHeaders {
            signature,
            event_type: "pull_request".to_string(),
            delivery_id: delivery_id.to_string(),
        }
    }

    #[tokio::test]
    async fn signature_off_accepts() {
        let store = Arc::new(InMemoryDeliveryStore::default());
        let ingestor = WebhookIngestor::new(store, None, false);
        let outcome = ingestor
            .ingest(&headers("d1", None), &pull_request_body())
            .await
            .expect("ingest");
        assert!(matches!(outcome, IngestOutcome::Accepted { .. }));
    }

    #[tokio::test]
    async fn missing_signature_when_secret_set() {
        let store = Arc::new(InMemoryDeliveryStore::default());
        let ingestor = WebhookIngestor::new(store, Some(b"topsecret".to_vec()), false);
        let outcome = ingestor
            .ingest(&headers("d1", None), &pull_request_body())
            .await
            .expect("ingest");
        assert_eq!(outcome, IngestOutcome::SignatureMissing);
    }

    #[tokio::test]
    async fn bad_signature_rejected() {
        let store = Arc::new(InMemoryDeliveryStore::default());
        let ingestor = WebhookIngestor::new(store, Some(b"topsecret".to_vec()), false);
        let body = pull_request_body();
        let forged = sign(b"a different secret", &body);
        let outcome = ingestor
            .ingest(&headers("d1", Some(forged)), &body)
            .await
            .expect("ingest");
        assert_eq!(outcome, IngestOutcome::SignatureInvalid);
    }

    #[tokio::test]
    async fn forged_signature_on_unparseable_body_rejected_before_parse() {
        // Pins the verify-BEFORE-parse ordering: with an invalid-JSON body and a
        // bad signature, the outcome must be SignatureInvalid — a Malformed
        // error would mean the body was parsed first.
        let store = Arc::new(InMemoryDeliveryStore::default());
        let ingestor = WebhookIngestor::new(store, Some(b"topsecret".to_vec()), false);
        let body = b"this is not json {{{";
        let forged = sign(b"a different secret", body);
        let outcome = ingestor
            .ingest(&headers("d1", Some(forged)), body)
            .await
            .expect("ingest");
        assert_eq!(outcome, IngestOutcome::SignatureInvalid);
    }

    #[tokio::test]
    async fn empty_delivery_id_is_malformed_not_recorded() {
        let store = Arc::new(InMemoryDeliveryStore::default());
        let ingestor = WebhookIngestor::new(Arc::clone(&store) as _, None, false);
        let err = ingestor
            .ingest(&headers("", None), &pull_request_body())
            .await
            .expect_err("an empty delivery id must be malformed");
        assert!(matches!(err, WebhookError::Malformed(_)));
        // And a later real delivery must still be fresh — nothing was recorded.
        let outcome = ingestor
            .ingest(&headers("d-real", None), &pull_request_body())
            .await
            .expect("ingest");
        assert!(matches!(outcome, IngestOutcome::Accepted { .. }));
    }

    #[tokio::test]
    async fn replay_is_duplicate() {
        let store = Arc::new(InMemoryDeliveryStore::default());
        let ingestor = WebhookIngestor::new(store, None, false);
        let body = pull_request_body();
        let first = ingestor
            .ingest(&headers("dup", None), &body)
            .await
            .expect("ingest");
        assert!(matches!(first, IngestOutcome::Accepted { .. }));
        let second = ingestor
            .ingest(&headers("dup", None), &body)
            .await
            .expect("ingest");
        assert_eq!(second, IngestOutcome::Duplicate);
    }

    #[tokio::test]
    async fn trigger_defaults_false() {
        let store = Arc::new(InMemoryDeliveryStore::default());
        let ingestor = WebhookIngestor::new(store, None, false);
        assert!(!ingestor.should_trigger());
        let outcome = ingestor
            .ingest(&headers("d1", None), &pull_request_body())
            .await
            .expect("ingest");
        match outcome {
            IngestOutcome::Accepted { trigger, .. } => assert!(!trigger),
            other => panic!("expected accepted, got {other:?}"),
        }
    }
}
