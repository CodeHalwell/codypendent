//! Durable automation binding storage and querying.
//!
//! Automation bindings select a versioned workflow and describe when and how it may
//! be invoked by triggers or schedules. All mutations and queries are scoped to the
//! transport-derived [`PeerPrincipal`].

use std::str::FromStr;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use codypendent_protocol::{
    AutomationApprovalMode, AutomationBinding, AutomationBindingDraft, AutomationBindingId,
    AutomationBindingPage, AutomationBindingPatch, AutomationBindingQuery, CodypendentError,
    ConcurrencyPolicy, MissedRunPolicy, PageCursor, RepositoryId, TriggerSource,
    WebhookSignatureScheme, WorkflowId,
};
use croner::parser::CronParser;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use uuid::Uuid;

use crate::principal::PeerPrincipal;

const CURSOR_VERSION: u8 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct AutomationCursor {
    version: u8,
    query_hash: String,
    updated_at: String,
    id: String,
}

fn binding_not_found() -> CodypendentError {
    CodypendentError::new(
        "automation.binding-not-found",
        "automation binding is unavailable",
        false,
    )
}

fn database_error(error: impl std::fmt::Display) -> CodypendentError {
    CodypendentError::new("automation.database-error", error.to_string(), true)
}

fn invalid_cursor() -> CodypendentError {
    CodypendentError::new(
        "automation.invalid-cursor",
        "the automation query cursor is invalid",
        false,
    )
}

fn invalid_request(message: impl Into<String>) -> CodypendentError {
    CodypendentError::new("automation.invalid-request", message.into(), false)
}

fn name_collision(name: &str) -> CodypendentError {
    CodypendentError::new(
        "automation.name-collision",
        format!("an automation binding named '{name}' already exists"),
        false,
    )
}

fn repository_not_found(repository_id: RepositoryId) -> CodypendentError {
    CodypendentError::new(
        "workspace.repository-not-found",
        format!("repository {repository_id} is not accessible"),
        false,
    )
}

fn approval_required(receipt: &str) -> CodypendentError {
    CodypendentError::new(
        "policy.approval-required",
        format!("approval receipt '{receipt}' does not exist or is invalid"),
        false,
    )
}

fn unsupported_payload(message: impl Into<String>) -> CodypendentError {
    CodypendentError::new("protocol.unsupported-payload", message.into(), false)
}

/// The next cron occurrence strictly after `after`, computed **in `timezone`**
/// and returned in UTC.
///
/// This is the ONE place the tree computes a cron occurrence. The scheduler
/// re-arms `next_fire_at` after every firing and must not carry a second copy of
/// this logic: the occurrence has to be found in the binding's own zone and only
/// then converted to UTC, or a DST transition silently shifts every later firing
/// by an hour. `automation_bindings.cron_timezone` exists precisely so this can
/// be redone from the original zone rather than from the stored UTC instant.
pub fn next_cron_occurrence_after(
    expression: &str,
    timezone: &str,
    after: DateTime<Utc>,
) -> Result<DateTime<Utc>, CodypendentError> {
    let tz: Tz = timezone
        .parse()
        .map_err(|_| invalid_request(format!("invalid cron timezone '{timezone}'")))?;
    // `sloppy_ranges(true)` keeps croner 2.x's lenient step forms (`0/10`,
    // `/10`) parseable: a binding whose expression was accepted when it was
    // created must still compute its next firing after an upgrade, and the
    // tightened OCPS default in croner 4 would otherwise refuse it here.
    let cron = CronParser::builder()
        .sloppy_ranges(true)
        .build()
        .parse(expression)
        .map_err(|e| invalid_request(format!("invalid cron expression '{expression}': {e}")))?;
    let after_in_tz = after.with_timezone(&tz);
    let next = cron
        .find_next_occurrence(&after_in_tz, false)
        .map_err(|e| invalid_request(format!("failed to compute next cron occurrence: {e}")))?;
    Ok(next.with_timezone(&Utc))
}

/// The stable [`WorkflowId`] for a workflow manifest's own id (`repair-github-check`).
///
/// `automation_bindings.workflow_id` is a `WorkflowId` **UUID**, but the only
/// production start path — `WorkflowStarter` over `WorkflowSourceRegistry` —
/// resolves a workflow by its manifest NAME, and no `workflow_definitions` table
/// exists anywhere in `migrations/` to map one to the other. Rather than invent
/// such a table with no writer, the mapping is made *derivable*: the id is the
/// first 16 bytes of a domain-separated SHA-256 of the manifest name, exactly as
/// [`codypendent_knowledge::stable_repository_id`] derives a `RepositoryId` from
/// a canonical checkout path. A binding therefore names a resolvable workflow
/// iff some manifest in the registry hashes to its `workflow_id`, and the
/// dispatcher can invert the map by hashing the registry's ids (there are tens
/// of manifests, not millions).
///
/// Deliberately NOT `Uuid::now_v7()`-shaped: this value must be reproducible
/// from the name alone, and the workspace pins `uuid` to `["v7", "serde"]` so
/// `new_v5` is unavailable.
#[must_use]
pub fn workflow_id_for_manifest_name(name: &str) -> WorkflowId {
    let digest = Sha256::digest(format!("codypendent:workflow-manifest:v1:{name}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    WorkflowId(Uuid::from_bytes(bytes))
}

struct ProjectedSource {
    source_type: &'static str,
    source_json: String,
    endpoint_id: Option<String>,
    cron_expression: Option<String>,
    cron_timezone: Option<String>,
    one_time_at: Option<String>,
    next_fire_at: Option<String>,
}

fn validate_and_project_source(
    source: &TriggerSource,
) -> Result<ProjectedSource, CodypendentError> {
    let source_json = serde_json::to_string(source)
        .map_err(|e| invalid_request(format!("failed to serialize trigger source: {e}")))?;

    match source {
        TriggerSource::Cron {
            expression,
            timezone,
        } => {
            // The scheduler re-arms this same column after every firing through
            // the SAME helper, so create/update and re-arm can never drift.
            let next_fire_at =
                Some(next_cron_occurrence_after(expression, timezone, Utc::now())?.to_rfc3339());

            Ok(ProjectedSource {
                source_type: "cron",
                source_json,
                endpoint_id: None,
                cron_expression: Some(expression.clone()),
                cron_timezone: Some(timezone.clone()),
                one_time_at: None,
                next_fire_at,
            })
        }
        TriggerSource::OneTime { at } => {
            let at_str = at.to_rfc3339();
            Ok(ProjectedSource {
                source_type: "one_time",
                source_json,
                endpoint_id: None,
                cron_expression: None,
                cron_timezone: None,
                one_time_at: Some(at_str.clone()),
                next_fire_at: Some(at_str),
            })
        }
        TriggerSource::GitHubWebhook { endpoint_id, .. } => {
            if endpoint_id.trim().is_empty() {
                return Err(invalid_request("endpoint_id cannot be empty"));
            }
            Ok(ProjectedSource {
                source_type: "github_webhook",
                source_json,
                endpoint_id: Some(endpoint_id.clone()),
                cron_expression: None,
                cron_timezone: None,
                one_time_at: None,
                next_fire_at: None,
            })
        }
        TriggerSource::SignedWebhook {
            endpoint_id,
            signing_key_ref,
            signature,
        } => {
            if endpoint_id.trim().is_empty() {
                return Err(invalid_request("endpoint_id cannot be empty"));
            }
            if signing_key_ref.trim().is_empty() {
                return Err(invalid_request("signing_key_ref cannot be empty"));
            }
            if matches!(signature, WebhookSignatureScheme::Unknown) {
                return Err(unsupported_payload("unknown webhook signature scheme"));
            }
            Ok(ProjectedSource {
                source_type: "signed_webhook",
                source_json,
                endpoint_id: Some(endpoint_id.clone()),
                cron_expression: None,
                cron_timezone: None,
                one_time_at: None,
                next_fire_at: None,
            })
        }
        TriggerSource::CiFailure { .. } => Ok(ProjectedSource {
            source_type: "ci_failure",
            source_json,
            endpoint_id: None,
            cron_expression: None,
            cron_timezone: None,
            one_time_at: None,
            next_fire_at: None,
        }),
        TriggerSource::RepositoryChange => Ok(ProjectedSource {
            source_type: "repository_change",
            source_json,
            endpoint_id: None,
            cron_expression: None,
            cron_timezone: None,
            one_time_at: None,
            next_fire_at: None,
        }),
        TriggerSource::CodeGraphChange => Ok(ProjectedSource {
            source_type: "code_graph_change",
            source_json,
            endpoint_id: None,
            cron_expression: None,
            cron_timezone: None,
            one_time_at: None,
            next_fire_at: None,
        }),
        TriggerSource::DependencyAlert { .. } => Ok(ProjectedSource {
            source_type: "dependency_alert",
            source_json,
            endpoint_id: None,
            cron_expression: None,
            cron_timezone: None,
            one_time_at: None,
            next_fire_at: None,
        }),
        TriggerSource::Manual => Ok(ProjectedSource {
            source_type: "manual",
            source_json,
            endpoint_id: None,
            cron_expression: None,
            cron_timezone: None,
            one_time_at: None,
            next_fire_at: None,
        }),
        TriggerSource::Api => Ok(ProjectedSource {
            source_type: "api",
            source_json,
            endpoint_id: None,
            cron_expression: None,
            cron_timezone: None,
            one_time_at: None,
            next_fire_at: None,
        }),
        // Fail closed: `TriggerSource` is `#[non_exhaustive]`, so a newer client
        // can send a variant this daemon has never heard of. Refuse to project it
        // rather than persisting a trigger whose firing semantics are unknown.
        TriggerSource::Unknown | _ => Err(unsupported_payload("unknown trigger source variant")),
    }
}

struct ProjectedInvocation {
    invocation_json: String,
    dedup_window_seconds: i64,
    concurrency: &'static str,
    missed_run: &'static str,
    missed_run_max_occurrences: Option<i64>,
    retry_max_attempts: i64,
    retry_initial_delay_seconds: i64,
    retry_backoff_multiplier: i64,
    retry_max_delay_seconds: Option<i64>,
    budget_wall_time_seconds: Option<i64>,
    budget_tool_calls: Option<i64>,
    budget_tokens: Option<i64>,
    budget_cost_micros: Option<i64>,
    approval_mode: &'static str,
    approval_receipt: Option<String>,
}

async fn validate_and_project_invocation(
    pool: &SqlitePool,
    invocation: &codypendent_protocol::InvocationPolicy,
) -> Result<ProjectedInvocation, CodypendentError> {
    let invocation_json = serde_json::to_string(invocation)
        .map_err(|e| invalid_request(format!("failed to serialize invocation policy: {e}")))?;

    let concurrency = match invocation.concurrency {
        ConcurrencyPolicy::Allow => "allow",
        ConcurrencyPolicy::Skip => "skip",
        ConcurrencyPolicy::Queue => "queue",
        ConcurrencyPolicy::Replace => "replace",
        // Fail closed: an unrecognized concurrency policy is rejected rather than
        // silently downgraded to the permissive `allow`.
        ConcurrencyPolicy::Unknown | _ => {
            return Err(unsupported_payload("unknown concurrency policy"))
        }
    };

    let (missed_run, missed_run_max_occurrences) = match invocation.missed_run {
        MissedRunPolicy::Skip => ("skip", None),
        MissedRunPolicy::RunOnce => ("run_once", None),
        MissedRunPolicy::CatchUp { max_occurrences } => {
            if max_occurrences == 0 {
                return Err(invalid_request(
                    "missed_run catch_up max_occurrences must be > 0",
                ));
            }
            ("catch_up", Some(i64::from(max_occurrences)))
        }
        // Fail closed: an unrecognized missed-run policy is rejected rather than
        // defaulting to a catch-up that could fire an unbounded backlog.
        MissedRunPolicy::Unknown | _ => {
            return Err(unsupported_payload("unknown missed_run policy"))
        }
    };

    if invocation.retry.backoff_multiplier < 1 {
        return Err(invalid_request("retry backoff_multiplier must be >= 1"));
    }

    let (approval_mode, approval_receipt) = match &invocation.approval_mode {
        AutomationApprovalMode::Inherit => ("inherit", None),
        AutomationApprovalMode::AlwaysRequire => ("always_require", None),
        AutomationApprovalMode::PolicyDriven => ("policy_driven", None),
        AutomationApprovalMode::Preapproved { approval_receipt } => {
            if approval_receipt.trim().is_empty() {
                return Err(invalid_request(
                    "preapproved approval_receipt cannot be empty",
                ));
            }
            // Verify that the receipt actually exists in the approvals store
            let exists: Option<(String,)> =
                sqlx::query_as("SELECT id FROM approvals WHERE id = ? LIMIT 1")
                    .bind(approval_receipt)
                    .fetch_optional(pool)
                    .await
                    .map_err(database_error)?;

            if exists.is_none() {
                return Err(approval_required(approval_receipt));
            }

            ("preapproved", Some(approval_receipt.clone()))
        }
        // Fail closed: an unrecognized approval mode is rejected outright. It must
        // never fall through to `inherit` (or anything weaker than
        // `always_require`) — that would let a newer client's unknown mode be
        // stored as a laxer gate than it names.
        AutomationApprovalMode::Unknown | _ => {
            return Err(unsupported_payload("unknown approval mode"))
        }
    };

    let (budget_wall, budget_tools, budget_tokens, budget_cost) =
        if let Some(ref budget) = invocation.budget_ceiling {
            if let Some(0) = budget.wall_time_seconds {
                return Err(invalid_request(
                    "budget wall_time_seconds must be > 0 when set",
                ));
            }
            if let Some(0) = budget.tool_calls {
                return Err(invalid_request("budget tool_calls must be > 0 when set"));
            }
            if let Some(0) = budget.tokens {
                return Err(invalid_request("budget tokens must be > 0 when set"));
            }
            if let Some(0) = budget.cost_micros {
                return Err(invalid_request("budget cost_micros must be > 0 when set"));
            }
            (
                budget.wall_time_seconds.map(|v| v as i64),
                budget.tool_calls.map(|v| v as i64),
                budget.tokens.map(|v| v as i64),
                budget.cost_micros.map(|v| v as i64),
            )
        } else {
            (None, None, None, None)
        };

    Ok(ProjectedInvocation {
        invocation_json,
        dedup_window_seconds: invocation.deduplication.window_seconds as i64,
        concurrency,
        missed_run,
        missed_run_max_occurrences,
        retry_max_attempts: i64::from(invocation.retry.max_attempts),
        retry_initial_delay_seconds: invocation.retry.initial_delay_seconds as i64,
        retry_backoff_multiplier: i64::from(invocation.retry.backoff_multiplier),
        retry_max_delay_seconds: invocation.retry.max_delay_seconds.map(|d| d as i64),
        budget_wall_time_seconds: budget_wall,
        budget_tool_calls: budget_tools,
        budget_tokens,
        budget_cost_micros: budget_cost,
        approval_mode,
        approval_receipt,
    })
}

async fn resolve_repository_path(
    pool: &SqlitePool,
    principal: PeerPrincipal,
    repository_id: RepositoryId,
) -> Result<String, CodypendentError> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT repository FROM sessions \
         WHERE repository_id = ? \
           AND COALESCE(owner_uid, ?) = ? \
           AND repository IS NOT NULL \
         ORDER BY updated_at DESC LIMIT 1",
    )
    .bind(repository_id.to_string())
    .bind(i64::from(principal.uid()))
    .bind(i64::from(principal.uid()))
    .fetch_optional(pool)
    .await
    .map_err(database_error)?;

    match row {
        Some((path,)) => Ok(path),
        None => Err(repository_not_found(repository_id)),
    }
}

fn parse_binding_row(row: &sqlx::sqlite::SqliteRow) -> Result<AutomationBinding, CodypendentError> {
    let id_str: String = row
        .try_get("id")
        .map_err(|e| database_error(format!("failed to read id: {e}")))?;
    let id = AutomationBindingId::from_str(&id_str)
        .map_err(|e| database_error(format!("invalid binding id '{id_str}': {e}")))?;

    let name: String = row
        .try_get("name")
        .map_err(|e| database_error(format!("failed to read name: {e}")))?;
    let source_json: String = row
        .try_get("source_json")
        .map_err(|e| database_error(format!("failed to read source_json: {e}")))?;
    let source: TriggerSource = serde_json::from_str(&source_json)
        .map_err(|e| database_error(format!("failed to parse source_json: {e}")))?;

    let workflow_id_str: String = row
        .try_get("workflow_id")
        .map_err(|e| database_error(format!("failed to read workflow_id: {e}")))?;
    let workflow_id = WorkflowId::from_str(&workflow_id_str)
        .map_err(|e| database_error(format!("invalid workflow id '{workflow_id_str}': {e}")))?;

    let workflow_version: String = row
        .try_get("workflow_version")
        .map_err(|e| database_error(format!("failed to read workflow_version: {e}")))?;

    let repository_id_str: String = row
        .try_get("repository_id")
        .map_err(|e| database_error(format!("failed to read repository_id: {e}")))?;
    let repository_id = RepositoryId::from_str(&repository_id_str)
        .map_err(|e| database_error(format!("invalid repository id '{repository_id_str}': {e}")))?;

    let filters_json: String = row
        .try_get("filters_json")
        .map_err(|e| database_error(format!("failed to read filters_json: {e}")))?;
    let filters = serde_json::from_str(&filters_json)
        .map_err(|e| database_error(format!("failed to parse filters_json: {e}")))?;

    let invocation_json: String = row
        .try_get("invocation_json")
        .map_err(|e| database_error(format!("failed to read invocation_json: {e}")))?;
    let invocation = serde_json::from_str(&invocation_json)
        .map_err(|e| database_error(format!("failed to parse invocation_json: {e}")))?;

    let enabled_int: i64 = row
        .try_get("enabled")
        .map_err(|e| database_error(format!("failed to read enabled: {e}")))?;
    let enabled = enabled_int != 0;

    let created_at_str: String = row
        .try_get("created_at")
        .map_err(|e| database_error(format!("failed to read created_at: {e}")))?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| database_error(format!("invalid created_at '{created_at_str}': {e}")))?
        .with_timezone(&Utc);

    let updated_at_str: String = row
        .try_get("updated_at")
        .map_err(|e| database_error(format!("failed to read updated_at: {e}")))?;
    let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
        .map_err(|e| database_error(format!("invalid updated_at '{updated_at_str}': {e}")))?
        .with_timezone(&Utc);

    Ok(AutomationBinding {
        id,
        definition: AutomationBindingDraft {
            name,
            source,
            workflow_id,
            workflow_version,
            repository_id,
            filters,
            invocation,
            enabled,
        },
        created_at,
        updated_at,
    })
}

fn query_hash(
    principal_uid: u32,
    query: &AutomationBindingQuery,
) -> Result<String, CodypendentError> {
    let payload = serde_json::to_vec(&(
        principal_uid,
        query.repository_id.as_ref().map(|r| r.to_string()),
        query.workflow_id.as_ref().map(|w| w.to_string()),
        query.enabled,
    ))
    .map_err(|e| invalid_request(e.to_string()))?;
    Ok(hex::encode(Sha256::digest(payload)))
}

fn encode_cursor(
    last: &AutomationBinding,
    query_hash: &str,
) -> Result<PageCursor, CodypendentError> {
    let cursor = AutomationCursor {
        version: CURSOR_VERSION,
        query_hash: query_hash.to_string(),
        updated_at: last.updated_at.to_rfc3339(),
        id: last.id.to_string(),
    };
    let bytes = serde_json::to_vec(&cursor).map_err(|e| database_error(e.to_string()))?;
    Ok(PageCursor(URL_SAFE_NO_PAD.encode(bytes)))
}

fn decode_cursor(
    cursor: &PageCursor,
    expected_query_hash: &str,
) -> Result<AutomationCursor, CodypendentError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(&cursor.0)
        .map_err(|_| invalid_cursor())?;
    let decoded: AutomationCursor = serde_json::from_slice(&bytes).map_err(|_| invalid_cursor())?;
    if decoded.version != CURSOR_VERSION || decoded.query_hash != expected_query_hash {
        return Err(invalid_cursor());
    }
    Ok(decoded)
}

/// Store for managing automation bindings.
#[derive(Debug, Clone)]
pub struct AutomationStore {
    pool: SqlitePool,
}

impl AutomationStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn create_binding(
        &self,
        principal: PeerPrincipal,
        draft: AutomationBindingDraft,
    ) -> Result<AutomationBinding, CodypendentError> {
        create_binding(&self.pool, principal, draft).await
    }

    pub async fn get_binding(
        &self,
        principal: PeerPrincipal,
        id: AutomationBindingId,
    ) -> Result<AutomationBinding, CodypendentError> {
        get_binding(&self.pool, principal, id).await
    }

    pub async fn list_bindings(
        &self,
        principal: PeerPrincipal,
        query: &AutomationBindingQuery,
    ) -> Result<AutomationBindingPage, CodypendentError> {
        list_bindings(&self.pool, principal, query).await
    }

    pub async fn update_binding(
        &self,
        principal: PeerPrincipal,
        id: AutomationBindingId,
        patch: &AutomationBindingPatch,
    ) -> Result<AutomationBinding, CodypendentError> {
        update_binding(&self.pool, principal, id, patch).await
    }

    pub async fn delete_binding(
        &self,
        principal: PeerPrincipal,
        id: AutomationBindingId,
    ) -> Result<(), CodypendentError> {
        delete_binding(&self.pool, principal, id).await
    }
}

/// Create a new automation binding owned by `principal`.
pub async fn create_binding(
    pool: &SqlitePool,
    principal: PeerPrincipal,
    draft: AutomationBindingDraft,
) -> Result<AutomationBinding, CodypendentError> {
    if draft.name.trim().is_empty() {
        return Err(invalid_request("binding name cannot be empty"));
    }

    let repository_path = resolve_repository_path(pool, principal, draft.repository_id).await?;

    let projected_source = validate_and_project_source(&draft.source)?;
    let projected_invocation = validate_and_project_invocation(pool, &draft.invocation).await?;

    let id = AutomationBindingId::new();
    let owner_uid = i64::from(principal.uid());
    let now = Utc::now();
    let created_at = now.to_rfc3339();
    let updated_at = created_at.clone();

    let filters_json = serde_json::to_string(&draft.filters)
        .map_err(|e| invalid_request(format!("failed to serialize filters: {e}")))?;

    let enabled_int: i64 = if draft.enabled { 1 } else { 0 };

    let res = sqlx::query(
        "INSERT INTO automation_bindings (
            id, owner_uid, name, source_type, source_json, endpoint_id,
            cron_expression, cron_timezone, one_time_at, next_fire_at, last_fire_at,
            workflow_id, workflow_version, repository_id, repository_path,
            filters_json, invocation_json, dedup_window_seconds, concurrency,
            missed_run, missed_run_max_occurrences, retry_max_attempts,
            retry_initial_delay_seconds, retry_backoff_multiplier, retry_max_delay_seconds,
            budget_wall_time_seconds, budget_tool_calls, budget_tokens, budget_cost_micros,
            approval_mode, approval_receipt, enabled, created_at, updated_at
        ) VALUES (
            ?, ?, ?, ?, ?, ?,
            ?, ?, ?, ?, NULL,
            ?, ?, ?, ?,
            ?, ?, ?, ?,
            ?, ?, ?,
            ?, ?, ?,
            ?, ?, ?, ?,
            ?, ?, ?, ?, ?
        )",
    )
    .bind(id.to_string())
    .bind(owner_uid)
    .bind(&draft.name)
    .bind(projected_source.source_type)
    .bind(&projected_source.source_json)
    .bind(projected_source.endpoint_id)
    .bind(projected_source.cron_expression)
    .bind(projected_source.cron_timezone)
    .bind(projected_source.one_time_at)
    .bind(projected_source.next_fire_at)
    .bind(draft.workflow_id.to_string())
    .bind(&draft.workflow_version)
    .bind(draft.repository_id.to_string())
    .bind(repository_path)
    .bind(filters_json)
    .bind(&projected_invocation.invocation_json)
    .bind(projected_invocation.dedup_window_seconds)
    .bind(projected_invocation.concurrency)
    .bind(projected_invocation.missed_run)
    .bind(projected_invocation.missed_run_max_occurrences)
    .bind(projected_invocation.retry_max_attempts)
    .bind(projected_invocation.retry_initial_delay_seconds)
    .bind(projected_invocation.retry_backoff_multiplier)
    .bind(projected_invocation.retry_max_delay_seconds)
    .bind(projected_invocation.budget_wall_time_seconds)
    .bind(projected_invocation.budget_tool_calls)
    .bind(projected_invocation.budget_tokens)
    .bind(projected_invocation.budget_cost_micros)
    .bind(projected_invocation.approval_mode)
    .bind(projected_invocation.approval_receipt)
    .bind(enabled_int)
    .bind(&created_at)
    .bind(&updated_at)
    .execute(pool)
    .await;

    match res {
        Ok(_) => Ok(AutomationBinding {
            id,
            definition: draft,
            created_at: now,
            updated_at: now,
        }),
        Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
            Err(name_collision(&draft.name))
        }
        Err(err) => Err(database_error(err)),
    }
}

/// Retrieve an automation binding by `id`, ensuring it belongs to `principal`.
pub async fn get_binding(
    pool: &SqlitePool,
    principal: PeerPrincipal,
    id: AutomationBindingId,
) -> Result<AutomationBinding, CodypendentError> {
    let owner_uid = i64::from(principal.uid());
    let row = sqlx::query(
        "SELECT id, owner_uid, name, source_json, workflow_id, workflow_version,
                repository_id, filters_json, invocation_json, enabled, created_at, updated_at
         FROM automation_bindings
         WHERE id = ? AND owner_uid = ?",
    )
    .bind(id.to_string())
    .bind(owner_uid)
    .fetch_optional(pool)
    .await
    .map_err(database_error)?;

    match row {
        Some(row) => parse_binding_row(&row),
        None => Err(binding_not_found()),
    }
}

/// List automation bindings owned by `principal`, with optional filters and keyset pagination.
pub async fn list_bindings(
    pool: &SqlitePool,
    principal: PeerPrincipal,
    query: &AutomationBindingQuery,
) -> Result<AutomationBindingPage, CodypendentError> {
    let owner_uid = i64::from(principal.uid());
    let limit = query.limit.unwrap_or(50).clamp(1, 200) as i64;
    let expected_hash = query_hash(principal.uid(), query)?;

    let cursor_data = if let Some(ref cursor) = query.cursor {
        Some(decode_cursor(cursor, &expected_hash)?)
    } else {
        None
    };

    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        "SELECT id, owner_uid, name, source_json, workflow_id, workflow_version,
                repository_id, filters_json, invocation_json, enabled, created_at, updated_at
         FROM automation_bindings
         WHERE owner_uid = ",
    );
    qb.push_bind(owner_uid);

    if let Some(ref repo_id) = query.repository_id {
        qb.push(" AND repository_id = ");
        qb.push_bind(repo_id.to_string());
    }

    if let Some(ref wf_id) = query.workflow_id {
        qb.push(" AND workflow_id = ");
        qb.push_bind(wf_id.to_string());
    }

    if let Some(enabled) = query.enabled {
        qb.push(" AND enabled = ");
        qb.push_bind(if enabled { 1i64 } else { 0i64 });
    }

    if let Some(cursor) = cursor_data {
        qb.push(" AND (updated_at < ");
        qb.push_bind(cursor.updated_at.clone());
        qb.push(" OR (updated_at = ");
        qb.push_bind(cursor.updated_at);
        qb.push(" AND id < ");
        qb.push_bind(cursor.id);
        qb.push("))");
    }

    qb.push(" ORDER BY updated_at DESC, id DESC LIMIT ");
    qb.push_bind(limit + 1);

    let rows = qb.build().fetch_all(pool).await.map_err(database_error)?;

    let mut items = Vec::new();
    let has_more = rows.len() as i64 > limit;
    let take_count = if has_more { limit as usize } else { rows.len() };

    for row in rows.iter().take(take_count) {
        items.push(parse_binding_row(row)?);
    }

    let next_cursor = if has_more {
        if let Some(last) = items.last() {
            Some(encode_cursor(last, &expected_hash)?)
        } else {
            None
        }
    } else {
        None
    };

    Ok(AutomationBindingPage { items, next_cursor })
}

/// Update an existing automation binding with a sparse patch.
pub async fn update_binding(
    pool: &SqlitePool,
    principal: PeerPrincipal,
    id: AutomationBindingId,
    patch: &AutomationBindingPatch,
) -> Result<AutomationBinding, CodypendentError> {
    let existing = get_binding(pool, principal, id).await?;

    let name = patch.name.as_ref().unwrap_or(&existing.definition.name);
    if name.trim().is_empty() {
        return Err(invalid_request("binding name cannot be empty"));
    }

    let source = patch.source.as_ref().unwrap_or(&existing.definition.source);
    let workflow_id = patch.workflow_id.unwrap_or(existing.definition.workflow_id);
    let workflow_version = patch
        .workflow_version
        .as_ref()
        .unwrap_or(&existing.definition.workflow_version);
    let repository_id = patch
        .repository_id
        .unwrap_or(existing.definition.repository_id);
    let filters = patch
        .filters
        .as_ref()
        .unwrap_or(&existing.definition.filters);
    let invocation = patch
        .invocation
        .as_ref()
        .unwrap_or(&existing.definition.invocation);
    let enabled = patch.enabled.unwrap_or(existing.definition.enabled);

    let repository_path = resolve_repository_path(pool, principal, repository_id).await?;

    let projected_source = validate_and_project_source(source)?;
    let projected_invocation = validate_and_project_invocation(pool, invocation).await?;

    // A patch that does NOT change the source must not move the schedule. This
    // UPDATE rewrites every projected column, and `validate_and_project_source`
    // recomputes `next_fire_at` from *now*; letting that land on an
    // `enabled`-only or `name`-only patch would push a due 02:00 job to the next
    // 02:00 every time anything about the binding was edited — and would race
    // the scheduler's compare-and-swap on the exact value it read. So the stored
    // occurrence is carried forward verbatim unless the source itself changed.
    let next_fire_at = if patch.source.is_some() {
        projected_source.next_fire_at
    } else {
        let stored: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT next_fire_at FROM automation_bindings WHERE id = ? AND owner_uid = ?",
        )
        .bind(id.to_string())
        .bind(i64::from(principal.uid()))
        .fetch_optional(pool)
        .await
        .map_err(database_error)?;
        match stored {
            Some((value,)) => value,
            // The row vanished between the `get_binding` above and here; the
            // UPDATE below will report zero rows and refuse identically.
            None => return Err(binding_not_found()),
        }
    };

    let filters_json = serde_json::to_string(filters)
        .map_err(|e| invalid_request(format!("failed to serialize filters: {e}")))?;

    let enabled_int: i64 = if enabled { 1 } else { 0 };
    let now = Utc::now();
    let updated_at = now.to_rfc3339();
    let owner_uid = i64::from(principal.uid());

    let res = sqlx::query(
        "UPDATE automation_bindings SET
            name = ?,
            source_type = ?,
            source_json = ?,
            endpoint_id = ?,
            cron_expression = ?,
            cron_timezone = ?,
            one_time_at = ?,
            next_fire_at = ?,
            workflow_id = ?,
            workflow_version = ?,
            repository_id = ?,
            repository_path = ?,
            filters_json = ?,
            invocation_json = ?,
            dedup_window_seconds = ?,
            concurrency = ?,
            missed_run = ?,
            missed_run_max_occurrences = ?,
            retry_max_attempts = ?,
            retry_initial_delay_seconds = ?,
            retry_backoff_multiplier = ?,
            retry_max_delay_seconds = ?,
            budget_wall_time_seconds = ?,
            budget_tool_calls = ?,
            budget_tokens = ?,
            budget_cost_micros = ?,
            approval_mode = ?,
            approval_receipt = ?,
            enabled = ?,
            updated_at = ?
         WHERE id = ? AND owner_uid = ?",
    )
    .bind(name)
    .bind(projected_source.source_type)
    .bind(&projected_source.source_json)
    .bind(projected_source.endpoint_id)
    .bind(projected_source.cron_expression)
    .bind(projected_source.cron_timezone)
    .bind(projected_source.one_time_at)
    .bind(next_fire_at)
    .bind(workflow_id.to_string())
    .bind(workflow_version)
    .bind(repository_id.to_string())
    .bind(repository_path)
    .bind(filters_json)
    .bind(&projected_invocation.invocation_json)
    .bind(projected_invocation.dedup_window_seconds)
    .bind(projected_invocation.concurrency)
    .bind(projected_invocation.missed_run)
    .bind(projected_invocation.missed_run_max_occurrences)
    .bind(projected_invocation.retry_max_attempts)
    .bind(projected_invocation.retry_initial_delay_seconds)
    .bind(projected_invocation.retry_backoff_multiplier)
    .bind(projected_invocation.retry_max_delay_seconds)
    .bind(projected_invocation.budget_wall_time_seconds)
    .bind(projected_invocation.budget_tool_calls)
    .bind(projected_invocation.budget_tokens)
    .bind(projected_invocation.budget_cost_micros)
    .bind(projected_invocation.approval_mode)
    .bind(projected_invocation.approval_receipt)
    .bind(enabled_int)
    .bind(&updated_at)
    .bind(id.to_string())
    .bind(owner_uid)
    .execute(pool)
    .await;

    match res {
        Ok(result) => {
            if result.rows_affected() == 0 {
                return Err(binding_not_found());
            }
            Ok(AutomationBinding {
                id,
                definition: AutomationBindingDraft {
                    name: name.clone(),
                    source: source.clone(),
                    workflow_id,
                    workflow_version: workflow_version.clone(),
                    repository_id,
                    filters: filters.clone(),
                    invocation: invocation.clone(),
                    enabled,
                },
                created_at: existing.created_at,
                updated_at: now,
            })
        }
        Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
            Err(name_collision(name))
        }
        Err(err) => Err(database_error(err)),
    }
}

/// Delete an automation binding by `id`, ensuring it belongs to `principal`.
pub async fn delete_binding(
    pool: &SqlitePool,
    principal: PeerPrincipal,
    id: AutomationBindingId,
) -> Result<(), CodypendentError> {
    let owner_uid = i64::from(principal.uid());
    let res = sqlx::query("DELETE FROM automation_bindings WHERE id = ? AND owner_uid = ?")
        .bind(id.to_string())
        .bind(owner_uid)
        .execute(pool)
        .await
        .map_err(database_error)?;

    if res.rows_affected() == 0 {
        return Err(binding_not_found());
    }
    Ok(())
}
