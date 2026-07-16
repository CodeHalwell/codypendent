//! The scoped registry (Chapter 05, STEP 2.2).
//!
//! [`Registry`] is the governed CRUD surface over `registry_items`. It is
//! stateless — every method takes the [`SqlitePool`] per call, matching the
//! sibling managers in the daemon — and every authoritative write appends an
//! [`outbox`] row **in the same transaction** so an indexer crash can never
//! corrupt the authoritative rows (Chapter 06 index-outbox pattern).
//!
//! ## Scope and shadowing
//!
//! Identity is `(kind, name, scope)`, enforced by a unique index on
//! `(kind, name, scope_tier, scope_key)`. The *same* skill id may therefore be
//! registered at several scopes (a `User` skill and a `Workspace` skill of the
//! same name are distinct rows, both visible via [`list`](Registry::list)).
//! Selection resolves the winner by [`Scope::specificity`] — a more specific
//! scope shadows a broader one — via [`resolve_shadowed`]; the registry never
//! deletes the shadowed row.

use std::str::FromStr;

use chrono::{DateTime, Utc};
// Only `UserId` (a plain-string id) is named explicitly; the UUID-backed scope
// ids are reconstructed generically via `parse_scope_id`, their concrete type
// inferred from the `Scope` variant, so they need no import here.
use codypendent_protocol::{RegistryItemId, UserId};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::manifest::{self, ManifestError};
use crate::outbox::{self, KnowledgeIndexEvent};
use crate::types::{RegistryItem, RegistryItemKind, RegistryStatus, Scope, Version};

/// Every column of `registry_items`, in a fixed order shared by the SELECT
/// statements and [`from_row`].
const COLUMNS: &str = "id, kind, name, version, scope_json, scope_tier, scope_key, description, \
     intents_json, keywords_json, examples_json, input_schema_json, output_schema_json, \
     dependencies_json, permissions_json, risk, provenance_json, trust_json, trust_tier, \
     content_hash, status, executable, created_at, updated_at";

/// A structured registry error; raw `sqlx`/`serde`/manifest failures are wrapped,
/// never surfaced verbatim.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// A stored row could not be decoded (should never happen; the registry wrote
    /// it).
    #[error("corrupt registry row: {0}")]
    Corrupt(String),
    /// Loading a skill package failed.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

/// The governed registry over `registry_items`. Stateless: the pool is passed to
/// each method rather than held.
#[derive(Debug, Clone, Copy, Default)]
pub struct Registry;

impl Registry {
    /// A registry handle.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Insert or replace `item`, appending a `RegistryItemChanged` outbox row in
    /// the same transaction. Keyed on the item's `id`; a re-registration that
    /// wants to preserve identity across a new package load should reuse the
    /// existing id (see [`register_package`](Registry::register_package)).
    pub async fn upsert(
        &self,
        pool: &SqlitePool,
        item: &RegistryItem,
    ) -> Result<(), RegistryError> {
        let now = Utc::now();
        let mut tx = pool.begin().await?;
        sqlx::query(
            "INSERT OR REPLACE INTO registry_items \
             (id, kind, name, version, scope_json, scope_tier, scope_key, description, \
              intents_json, keywords_json, examples_json, input_schema_json, output_schema_json, \
              dependencies_json, permissions_json, risk, provenance_json, trust_json, trust_tier, \
              content_hash, status, executable, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(item.id.to_string())
        .bind(enum_as_db(&item.kind)?)
        .bind(&item.name)
        .bind(&item.version.0)
        .bind(scope_to_db(&item.scope)?)
        .bind(item.scope.tier())
        .bind(item.scope.key())
        .bind(&item.description)
        .bind(serde_json::to_string(&item.intents)?)
        .bind(serde_json::to_string(&item.keywords)?)
        .bind(serde_json::to_string(&item.examples)?)
        .bind(
            item.input_schema
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        )
        .bind(
            item.output_schema
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        )
        .bind(serde_json::to_string(&item.dependencies)?)
        .bind(serde_json::to_string(&item.permissions)?)
        .bind(enum_as_db(&item.risk)?)
        .bind(serde_json::to_string(&item.provenance)?)
        .bind(serde_json::to_string(&item.trust)?)
        .bind(enum_as_db(&item.trust.tier)?)
        .bind(&item.content_hash)
        .bind(enum_as_db(&item.status)?)
        .bind(i64::from(item.executable))
        .bind(item.created_at.to_rfc3339())
        .bind(now.to_rfc3339())
        .execute(&mut *tx)
        .await?;

        outbox::enqueue(
            &mut *tx,
            &KnowledgeIndexEvent::RegistryItemChanged(item.id),
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Fetch an item by id.
    pub async fn get(
        &self,
        pool: &SqlitePool,
        id: RegistryItemId,
    ) -> Result<Option<RegistryItem>, RegistryError> {
        let row = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM registry_items WHERE id = ?"
        ))
        .bind(id.to_string())
        .fetch_optional(pool)
        .await?;
        row.as_ref().map(from_row).transpose()
    }

    /// Fetch the single item with the given `(kind, name, scope)` identity, if
    /// any. A `None`-keyed scope (`System`) is matched with `scope_key IS NULL`.
    pub async fn by_identity(
        &self,
        pool: &SqlitePool,
        kind: RegistryItemKind,
        name: &str,
        scope: &Scope,
    ) -> Result<Option<RegistryItem>, RegistryError> {
        let kind_db = enum_as_db(&kind)?;
        let row = match scope.key() {
            Some(key) => {
                sqlx::query(&format!(
                    "SELECT {COLUMNS} FROM registry_items \
                 WHERE kind = ? AND name = ? AND scope_tier = ? AND scope_key = ?"
                ))
                .bind(kind_db)
                .bind(name)
                .bind(scope.tier())
                .bind(key)
                .fetch_optional(pool)
                .await?
            }
            None => {
                sqlx::query(&format!(
                    "SELECT {COLUMNS} FROM registry_items \
                 WHERE kind = ? AND name = ? AND scope_tier = ? AND scope_key IS NULL"
                ))
                .bind(kind_db)
                .bind(name)
                .bind(scope.tier())
                .fetch_optional(pool)
                .await?
            }
        };
        row.as_ref().map(from_row).transpose()
    }

    /// Every registered item, oldest first.
    pub async fn list(&self, pool: &SqlitePool) -> Result<Vec<RegistryItem>, RegistryError> {
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM registry_items ORDER BY created_at ASC, id ASC"
        ))
        .fetch_all(pool)
        .await?;
        rows.iter().map(from_row).collect()
    }

    /// Remove an item by id, appending a `RegistryItemChanged` outbox row in the
    /// same transaction (so derived indexes drop it too). Returns whether a row
    /// was removed.
    pub async fn remove(
        &self,
        pool: &SqlitePool,
        id: RegistryItemId,
    ) -> Result<bool, RegistryError> {
        let now = Utc::now();
        let mut tx = pool.begin().await?;
        let result = sqlx::query("DELETE FROM registry_items WHERE id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        let removed = result.rows_affected() > 0;
        if removed {
            outbox::enqueue(&mut *tx, &KnowledgeIndexEvent::RegistryItemChanged(id), now).await?;
        }
        tx.commit().await?;
        Ok(removed)
    }

    /// Load the skill package at `dir`, register it under `scope`, and return the
    /// stored item.
    ///
    /// Hash-change detection: if an item with the same `(kind, name, scope)`
    /// already exists at the **same version** but a **different `content_hash`**,
    /// the item is flagged [`RegistryStatus::Modified`] (a package file changed
    /// without a version bump — surfaced in the UI). Otherwise the manifest's
    /// declared status is used. An existing item's `id` and `created_at` are
    /// reused so the item's identity is stable across re-registration.
    pub async fn register_package(
        &self,
        pool: &SqlitePool,
        dir: &std::path::Path,
        scope: Scope,
    ) -> Result<RegistryItem, RegistryError> {
        let mut item = manifest::load_package(dir, scope)?;

        if let Some(existing) = self
            .by_identity(pool, item.kind, &item.name, &item.scope)
            .await?
        {
            item.id = existing.id;
            item.created_at = existing.created_at;
            if existing.version == item.version && existing.content_hash != item.content_hash {
                item.status = RegistryStatus::Modified;
            }
        }

        self.upsert(pool, &item).await?;
        Ok(item)
    }
}

/// Resolve the winning item among rows that share an identity across scopes: the
/// one with the highest [`Scope::specificity`] shadows the rest (a workspace
/// skill wins over a user skill of the same name). Ties (which the unique index
/// forbids for a *single* identity) keep the first seen. Returns `None` for an
/// empty slice.
///
/// Shadowing is a **selection** concern only — every candidate row remains
/// visible via [`Registry::list`]; this never removes anything.
#[must_use]
pub fn resolve_shadowed(candidates: &[RegistryItem]) -> Option<&RegistryItem> {
    candidates
        .iter()
        .max_by_key(|item| item.scope.specificity())
}

/// Decode a full `registry_items` row into a [`RegistryItem`].
fn from_row(row: &SqliteRow) -> Result<RegistryItem, RegistryError> {
    let id: String = row.try_get("id")?;
    let kind: String = row.try_get("kind")?;
    let scope_tier: String = row.try_get("scope_tier")?;
    let scope_key: Option<String> = row.try_get("scope_key")?;
    let version: String = row.try_get("version")?;
    let intents_json: String = row.try_get("intents_json")?;
    let keywords_json: String = row.try_get("keywords_json")?;
    let examples_json: String = row.try_get("examples_json")?;
    let input_schema_json: Option<String> = row.try_get("input_schema_json")?;
    let output_schema_json: Option<String> = row.try_get("output_schema_json")?;
    let dependencies_json: String = row.try_get("dependencies_json")?;
    let permissions_json: String = row.try_get("permissions_json")?;
    let risk: String = row.try_get("risk")?;
    let provenance_json: String = row.try_get("provenance_json")?;
    let trust_json: String = row.try_get("trust_json")?;
    let status: String = row.try_get("status")?;
    let executable: i64 = row.try_get("executable")?;
    let created_at: String = row.try_get("created_at")?;
    let updated_at: String = row.try_get("updated_at")?;

    Ok(RegistryItem {
        id: RegistryItemId::from_str(&id)
            .map_err(|e| RegistryError::Corrupt(format!("id `{id}`: {e}")))?,
        kind: enum_from_db(&kind)?,
        name: row.try_get("name")?,
        version: Version(version),
        scope: scope_from_parts(&scope_tier, scope_key.as_deref())?,
        description: row.try_get("description")?,
        intents: serde_json::from_str(&intents_json)?,
        keywords: serde_json::from_str(&keywords_json)?,
        examples: serde_json::from_str(&examples_json)?,
        input_schema: input_schema_json
            .map(|s| serde_json::from_str(&s))
            .transpose()?,
        output_schema: output_schema_json
            .map(|s| serde_json::from_str(&s))
            .transpose()?,
        dependencies: serde_json::from_str(&dependencies_json)?,
        permissions: serde_json::from_str(&permissions_json)?,
        risk: enum_from_db(&risk)?,
        provenance: serde_json::from_str(&provenance_json)?,
        trust: serde_json::from_str(&trust_json)?,
        status: enum_from_db(&status)?,
        content_hash: row.try_get("content_hash")?,
        executable: executable != 0,
        created_at: parse_ts(&created_at, "created_at")?,
        updated_at: parse_ts(&updated_at, "updated_at")?,
    })
}

/// Serialize a scalar enum to the bare value stored in its column (the
/// snake_case variant name), e.g. `RegistryItemKind::Skill` → `"skill"`. These
/// enums serialize to a single JSON string, so trimming the quotes yields the
/// stored scalar; [`enum_from_db`] is its exact inverse.
fn enum_as_db<T: serde::Serialize>(value: &T) -> Result<String, RegistryError> {
    Ok(serde_json::to_string(value)?.trim_matches('"').to_string())
}

/// Parse a bare column value back into its scalar enum (inverse of
/// [`enum_as_db`]).
fn enum_from_db<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, RegistryError> {
    Ok(serde_json::from_str(&format!("\"{value}\""))?)
}

/// Parse an RFC 3339 timestamp column into a UTC instant.
fn parse_ts(value: &str, field: &str) -> Result<DateTime<Utc>, RegistryError> {
    DateTime::parse_from_rfc3339(value)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| RegistryError::Corrupt(format!("{field} `{value}`: {e}")))
}

/// Encode a [`Scope`] for the `scope_json` column.
///
/// [`Scope`] is an internally-tagged enum whose id-bearing variants wrap a scalar
/// id, a shape serde cannot serialize as a tagged map; the flattened
/// [`tier`](Scope::tier)/[`key`](Scope::key) projection is the authoritative,
/// round-trippable form (and the indexed one retrieval scope-filters on). This
/// mirrors it as `{"tier": …, "key": …}` — the tagged JSON the schema documents —
/// while [`scope_from_parts`] rebuilds the exact `Scope` from the two flattened
/// columns.
fn scope_to_db(scope: &Scope) -> Result<String, RegistryError> {
    let value = match scope.key() {
        Some(key) => serde_json::json!({ "tier": scope.tier(), "key": key }),
        None => serde_json::json!({ "tier": scope.tier() }),
    };
    Ok(serde_json::to_string(&value)?)
}

/// Rebuild a [`Scope`] from its flattened `scope_tier` / `scope_key` columns.
fn scope_from_parts(tier: &str, key: Option<&str>) -> Result<Scope, RegistryError> {
    let need = |tier: &str| {
        key.ok_or_else(|| RegistryError::Corrupt(format!("scope tier `{tier}` requires a key")))
    };
    let scope = match tier {
        "system" => Scope::System,
        "organization" => Scope::Organization(parse_scope_id(need(tier)?, tier)?),
        "user" => Scope::User(UserId(need(tier)?.to_string())),
        "workspace" => Scope::Workspace(parse_scope_id(need(tier)?, tier)?),
        "repository" => Scope::Repository(parse_scope_id(need(tier)?, tier)?),
        "branch" => Scope::Branch(parse_scope_id(need(tier)?, tier)?),
        "session" => Scope::Session(parse_scope_id(need(tier)?, tier)?),
        "task" => Scope::Task(parse_scope_id(need(tier)?, tier)?),
        other => {
            return Err(RegistryError::Corrupt(format!(
                "unknown scope tier `{other}`"
            )))
        }
    };
    Ok(scope)
}

/// Parse a UUID-backed scope id from its string column, tagging the tier on error.
fn parse_scope_id<T>(value: &str, tier: &str) -> Result<T, RegistryError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    T::from_str(value)
        .map_err(|e| RegistryError::Corrupt(format!("scope {tier} id `{value}`: {e}")))
}
