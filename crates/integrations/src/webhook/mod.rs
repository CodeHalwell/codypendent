//! Webhook ingestion (Phase 3 STEP 3.3).
//!
//! GitHub delivers events over HTTP. Ingestion is deliberately ordered so a
//! forged or replayed delivery can never reach the rest of the system:
//!
//! 1. [`verify`] checks the `X-Hub-Signature-256` HMAC **before any parsing** —
//!    an unsigned or mis-signed body is rejected without ever being deserialized.
//! 2. [`store`] records the `X-GitHub-Delivery` GUID; a redelivery (same GUID)
//!    is acknowledged but never processed a second time (replay idempotency).
//! 3. [`normalize`] turns the raw payload into a small internal
//!    [`NormalizedEvent`]; unknown event types degrade to `Other`.
//!
//! [`ingest::WebhookIngestor`] ties these together, and [`server`] is a minimal
//! hand-rolled localhost HTTP/1.1 listener that maps outcomes to status codes.
//! Workflows are only triggered when policy explicitly allows it (default off).

pub mod config;
pub mod ingest;
pub mod normalize;
pub mod server;
pub mod store;
pub mod verify;

pub use config::WebhooksConfig;
pub use ingest::{DeliveryHeaders, IngestOutcome, WebhookIngestor};
pub use normalize::NormalizedEvent;
pub use store::{DeliveryStore, InMemoryDeliveryStore, SqliteDeliveryStore};
pub use verify::{sign, verify_signature};

/// A failure during webhook ingestion.
#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    /// The delivery-idempotency store failed.
    #[error("webhook store error: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// A payload could not be (de)serialized.
    #[error("webhook serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// An I/O failure (reading a config file, socket, …).
    #[error("webhook I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The webhook configuration was invalid.
    #[error("webhook configuration error: {0}")]
    Config(String),
    /// The payload was malformed (e.g. invalid JSON body).
    #[error("malformed webhook payload: {0}")]
    Malformed(String),
}
