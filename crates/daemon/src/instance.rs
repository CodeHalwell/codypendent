//! Daemon instance identity.
//!
//! The single-row `daemon_instance` table proves that daemon state survives
//! restarts: the instance ID is created once and `boot_count` increments on
//! every boot.

use chrono::{DateTime, Utc};
use codypendent_protocol::DaemonInstanceId;
use sqlx::SqlitePool;

#[derive(Debug, Clone)]
pub struct InstanceRecord {
    pub instance_id: DaemonInstanceId,
    pub created_at: DateTime<Utc>,
    pub boot_count: i64,
}

/// Insert the instance row on first boot; increment `boot_count` on every
/// boot. Returns the current record.
pub async fn record_boot(pool: &SqlitePool) -> anyhow::Result<InstanceRecord> {
    let now = Utc::now();
    let existing: Option<(String, String, i64)> = sqlx::query_as(
        "SELECT instance_id, created_at, boot_count FROM daemon_instance WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;

    match existing {
        Some((instance_id, created_at, boot_count)) => {
            let boot_count = boot_count + 1;
            sqlx::query(
                "UPDATE daemon_instance SET boot_count = ?, last_started_at = ? WHERE id = 1",
            )
            .bind(boot_count)
            .bind(now.to_rfc3339())
            .execute(pool)
            .await?;
            Ok(InstanceRecord {
                instance_id: instance_id.parse()?,
                created_at: DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc),
                boot_count,
            })
        }
        None => {
            let instance_id = DaemonInstanceId::new();
            sqlx::query(
                "INSERT INTO daemon_instance (id, instance_id, created_at, boot_count, last_started_at) \
                 VALUES (1, ?, ?, 1, ?)",
            )
            .bind(instance_id.to_string())
            .bind(now.to_rfc3339())
            .bind(now.to_rfc3339())
            .execute(pool)
            .await?;
            Ok(InstanceRecord {
                instance_id,
                created_at: now,
                boot_count: 1,
            })
        }
    }
}
