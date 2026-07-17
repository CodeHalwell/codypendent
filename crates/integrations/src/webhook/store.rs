//! Replay-idempotency store for webhook deliveries.
//!
//! GitHub retries deliveries; every delivery carries a unique
//! `X-GitHub-Delivery` GUID. Recording that GUID *before* producing any internal
//! event makes ingestion replay-safe: a redelivered payload (same GUID) is
//! acknowledged but never normalized a second time. The GUID is the primary key
//! of `webhook_deliveries`, so the `INSERT OR IGNORE` itself is the idempotency
//! authority — a duplicate loses the insert (`rows_affected() == 0`).

use std::collections::HashSet;

use super::WebhookError;

/// Records webhook delivery GUIDs and reports whether each is being seen for the
/// first time.
#[async_trait::async_trait]
pub trait DeliveryStore: Send + Sync {
    /// Record `delivery_id`, returning `true` if this is the first time it has
    /// been seen and `false` if it was already recorded (a replay).
    async fn record_if_new(
        &self,
        delivery_id: &str,
        event_type: &str,
    ) -> Result<bool, WebhookError>;
}

/// The production [`DeliveryStore`], backed by the shared SQLite database.
pub struct SqliteDeliveryStore {
    pool: sqlx::SqlitePool,
}

impl SqliteDeliveryStore {
    /// Wrap a pool. The `webhook_deliveries` table is created by migration
    /// `0005_phase3.sql`.
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl DeliveryStore for SqliteDeliveryStore {
    async fn record_if_new(
        &self,
        delivery_id: &str,
        event_type: &str,
    ) -> Result<bool, WebhookError> {
        let result = sqlx::query(
            "INSERT OR IGNORE INTO webhook_deliveries (delivery_id, event_type, received_at) \
             VALUES (?, ?, ?)",
        )
        .bind(delivery_id)
        .bind(event_type)
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }
}

/// An in-memory [`DeliveryStore`] for tests: a `HashSet` guarded by an async
/// mutex, with the same first-seen semantics as the SQLite store.
#[derive(Default)]
pub struct InMemoryDeliveryStore {
    seen: tokio::sync::Mutex<HashSet<String>>,
}

#[async_trait::async_trait]
impl DeliveryStore for InMemoryDeliveryStore {
    async fn record_if_new(
        &self,
        delivery_id: &str,
        _event_type: &str,
    ) -> Result<bool, WebhookError> {
        let mut seen = self.seen.lock().await;
        Ok(seen.insert(delivery_id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_dedups_by_delivery_id() {
        let store = InMemoryDeliveryStore::default();
        assert!(store.record_if_new("guid-1", "push").await.unwrap());
        assert!(!store.record_if_new("guid-1", "push").await.unwrap());
        assert!(store.record_if_new("guid-2", "push").await.unwrap());
    }
}
