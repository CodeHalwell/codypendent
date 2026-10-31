//! The automation scheduler and trigger dispatcher — the writer that
//! `migrations/0044_automation.sql` was built for and never had.
//!
//! Before this module `automation_bindings` persisted policy that nothing read:
//! `next_fire_at` was written at create/update time and consulted only by a
//! test, and `automation_receipts`, `automation_attempts` and
//! `automation_leases` had **zero** INSERTs anywhere in the tree — the
//! at-least-once guarantee the schema documents had no implementation.
//!
//! # The three independent double-fire guards
//!
//! SQLite has no `SKIP LOCKED`, so exclusion is built the way
//! [`crate::ledger::append_next_event`] builds it: claim *inside* the write,
//! under `BEGIN IMMEDIATE`, and treat "zero rows" as "another writer won".
//!
//! 1. **The compare-and-swap.** A due binding is advanced with
//!    `UPDATE … WHERE id = ? AND enabled = 1 AND next_fire_at = <the exact value
//!    read>`. A concurrent tick, or a concurrent `update_binding`, changes that
//!    value and the loser's UPDATE affects zero rows. `enabled = 1` is in the
//!    WHERE, never a pre-read, so a binding disabled between the due query and
//!    the claim cannot fire.
//! 2. **The receipt.** `UNIQUE (binding_id, dedup_key)` makes the INSERT itself
//!    the reservation; `ON CONFLICT DO NOTHING RETURNING id` returning `None`
//!    means this occurrence (or this delivery) was already reserved.
//! 3. **The workflow store.** `workflow_idempotency_key` is handed verbatim to
//!    `create_run_idempotent_owned`, whose run id is
//!    `deterministic_run_id("owner:{uid}:{key}")` — so even a crash between the
//!    receipt commit and the start resolves to the SAME run on re-drive rather
//!    than a second one.
//!
//! [`automation_leases`] is the coarser outer guard the migration explicitly
//! demands instead of the in-process `DriveLockRegistry`: a row per binding held
//! by a `DaemonInstanceId`, with a monotonic `fence` re-checked inside every
//! claim transaction so a daemon that wakes after its lease expired discovers it
//! was fenced out before it writes.
//!
//! # What an attempt row means
//!
//! An `automation_attempts` row records a **dispatch decision made against a
//! reserved receipt** — which is exactly what its `outcome` vocabulary
//! (`filtered`, `denied`, `skipped_concurrency`, `queued`, `replaced`, `error`,
//! `started`) describes. It is never written speculatively: nothing is inserted
//! for a binding that was not claimed, and `outcome = 'started'` is written ONLY
//! after [`WorkflowStarter::start`] has returned a durable run id.
//!
//! # Fail-closed reads
//!
//! `concurrency`, `missed_run`, `approval_mode` and `source_type` come back out
//! of SQLite as TEXT. Every unrecognized value REFUSES, mirroring the `_ =>`
//! arms in [`crate::automation`]. An unknown `missed_run` must never fall
//! through to `catch_up` and fire an unbounded backlog, and an unknown
//! `approval_mode` must never fall through to `inherit`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use codypendent_protocol::{
    AutomationApprovalMode, ClientId, CodypendentError, ConcurrencyPolicy, DaemonInstanceId,
    MissedRunPolicy, RepositoryId, TriggerFilters, TriggerRetryPolicy, WorkflowId,
};
use serde_json::Value;
use sqlx::{Row, SqlitePool};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::automation::next_cron_occurrence_after;
use crate::workflows::{StartWorkflowRequest, WorkflowRunCeiling, WorkflowStarter};

/// How often [`AutomationScheduler::tick`] runs in production.
pub const TICK_INTERVAL: Duration = Duration::from_secs(30);

/// How long a binding lease is held. Comfortably longer than [`TICK_INTERVAL`]
/// so a slow tick does not fence itself out, and short enough that a crashed
/// daemon's bindings become claimable again without operator action.
pub const LEASE_TTL_SECONDS: i64 = 300;

/// An occurrence later than this behind `now` is a **missed** run, and the
/// binding's [`MissedRunPolicy`] decides what happens to it. Within the window a
/// merely-late tick fires normally.
pub const MISSED_RUN_GRACE_SECONDS: i64 = 120;

/// The hard ceiling on how many cron occurrences a single decision will walk.
/// A binding disabled for a year with a per-minute schedule must not spin the
/// daemon enumerating half a million instants: past this the decision REFUSES
/// rather than producing a partial answer.
pub const MAX_OCCURRENCE_SCAN: usize = 10_000;

/// Bindings claimed per tick. Bounded so one tick cannot monopolise the write
/// lock; the remainder is picked up by the next tick, still in due order.
pub const MAX_CLAIMS_PER_TICK: i64 = 32;

/// An in-flight attempt older than this is treated as interrupted by a crash and
/// closed, so its receipt becomes re-drivable.
const STALE_ATTEMPT_SECONDS: i64 = 900;

/// A reserved receipt untouched for this long with no in-flight attempt is
/// re-driven: this is the crash-between-reserve-and-dispatch recovery.
const ORPHAN_RECEIPT_SECONDS: i64 = 120;

/// Non-terminal workflow-run states (`migrations/0010_workflow_runs.sql`). The
/// concurrency policies join through `automation_receipts.workflow_run_id`
/// because `workflow_runs` carries no binding id — receipts are the only link.
const NON_TERMINAL_RUN_STATES: &str = "('pending', 'running', 'paused')";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn database_error(error: impl std::fmt::Display) -> CodypendentError {
    CodypendentError::new("automation.database-error", error.to_string(), true)
}

fn refuse(code: &'static str, message: impl Into<String>) -> CodypendentError {
    CodypendentError::new(code, message.into(), false)
}

// ---------------------------------------------------------------------------
// The assembly seam
// ---------------------------------------------------------------------------

/// Facts the daemon crate cannot derive on its own, supplied by the
/// `codypendentd` assembly — the same dependency inversion
/// [`WorkflowStarter`] uses.
///
/// The daemon cannot name `codypendent-workflow`'s `WorkflowSourceRegistry` (the
/// assembly owns workflow resolution) nor `codypendentd::scan::repository_id_for`
/// (which walks to a Git toplevel), so both cross this seam instead of being
/// re-implemented — a second copy of either would drift from the authority.
pub trait AutomationEnvironment: Send + Sync {
    /// Resolve a binding's `workflow_id` (+ its pinned `workflow_version`) to the
    /// manifest **name** [`StartWorkflowRequest::workflow_id`] takes, or refuse.
    ///
    /// Refusing is a first-class outcome: a binding may name a workflow that has
    /// since been deleted from every source, and firing "something else" would be
    /// far worse than not firing.
    ///
    /// `repository_path` is the binding's own checkout, because the assembly's
    /// sources include that checkout's `.codypendent/workflows` — a binding may
    /// name a workflow only that repository defines, and resolving without it
    /// would refuse a workflow that will in fact be started there.
    fn resolve_workflow(
        &self,
        workflow_id: WorkflowId,
        workflow_version: &str,
        repository_path: &Path,
    ) -> Result<String, CodypendentError>;

    /// Re-derive the [`RepositoryId`] of the checkout at `path`. `None` when the
    /// path no longer names a readable directory. The dispatcher compares this
    /// against the binding's stored `repository_id` and refuses on mismatch — a
    /// moved or renamed checkout must not silently run the automation against
    /// whatever now occupies the old path.
    fn repository_id_for(&self, path: &Path) -> Option<RepositoryId>;

    /// Whether `uid` still names an account this daemon may act as.
    ///
    /// `automation_bindings.owner_uid` has no foreign key and no existence check
    /// anywhere in the tree. The dispatcher synthesizes
    /// `PeerPrincipal::from_uid(owner_uid)` and must REFUSE rather than fall back
    /// to the daemon uid, so this is consulted before every start.
    fn owner_is_resolvable(&self, uid: u32) -> bool;
}

// ---------------------------------------------------------------------------
// The flattened trigger event
// ---------------------------------------------------------------------------

/// A normalized trigger occurrence, flattened to the string fields a binding's
/// [`TriggerFilters`] and `DeduplicationPolicy.identity_fields` name.
///
/// `codypendent-daemon` cannot depend on `codypendent-integrations`, so the
/// assembly flattens its `NormalizedEvent` into this before crossing the seam.
/// Keys are the normalizer's own field names (`repository`, `action`, `branch`,
/// `actor`, `number`, `check_name`, `status`).
#[derive(Debug, Clone, Default)]
pub struct TriggerEvent {
    /// The source-native event type (`pull_request`, `check_run`, `push`, …).
    pub event_type: String,
    /// The provider's delivery identity (`X-GitHub-Delivery`), where it has one.
    pub delivery_id: Option<String>,
    /// A fingerprint over the verified signature, as ingestion computes it.
    pub content_fingerprint: Option<String>,
    /// The normalized fields, by name.
    pub fields: BTreeMap<String, String>,
}

impl TriggerEvent {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// The binding record the dispatcher reads
// ---------------------------------------------------------------------------

/// The columns a firing needs. Deliberately NOT [`crate::automation::get_binding`]'s
/// projection: that one omits `repository_path`, `next_fire_at`, `last_fire_at`
/// and every projected policy column, because it serves the client contract
/// rather than dispatch.
#[derive(Debug, Clone)]
pub struct BindingRecord {
    pub id: String,
    pub owner_uid: u32,
    pub source_type: String,
    pub cron_expression: Option<String>,
    pub cron_timezone: Option<String>,
    pub one_time_at: Option<String>,
    pub next_fire_at: Option<String>,
    pub last_fire_at: Option<String>,
    pub workflow_id: WorkflowId,
    pub workflow_version: String,
    pub repository_id: RepositoryId,
    pub repository_path: String,
    pub filters: TriggerFilters,
    pub dedup_window_seconds: i64,
    /// Raw TEXT, parsed fail-closed by [`BindingRecord::policies`].
    concurrency_text: String,
    missed_run_text: String,
    missed_run_max_occurrences: Option<i64>,
    pub retry: TriggerRetryPolicy,
    approval_mode_text: String,
    approval_receipt: Option<String>,
    /// Any budget ceiling the binding declares. NULL stays NULL — a missing
    /// ceiling is never coerced to `0`, which would mean "no budget at all".
    pub budget: [Option<i64>; 4],
    pub identity_fields: Vec<String>,
}

/// The fail-closed parse of a binding's policy columns.
#[derive(Debug, Clone)]
pub struct BindingPolicies {
    pub concurrency: ConcurrencyPolicy,
    pub missed_run: MissedRunPolicy,
    pub approval_mode: AutomationApprovalMode,
}

const BINDING_COLUMNS: &str = "id, owner_uid, source_type, cron_expression, cron_timezone, \
     one_time_at, next_fire_at, last_fire_at, workflow_id, workflow_version, \
     repository_id, repository_path, filters_json, invocation_json, \
     dedup_window_seconds, concurrency, missed_run, missed_run_max_occurrences, \
     retry_max_attempts, retry_initial_delay_seconds, retry_backoff_multiplier, \
     retry_max_delay_seconds, budget_wall_time_seconds, budget_tool_calls, \
     budget_tokens, budget_cost_micros, approval_mode, approval_receipt";

impl BindingRecord {
    fn from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Self, CodypendentError> {
        let get_string = |name: &str| -> Result<String, CodypendentError> {
            row.try_get::<String, _>(name)
                .map_err(|e| database_error(format!("failed to read {name}: {e}")))
        };
        let get_opt = |name: &str| -> Result<Option<String>, CodypendentError> {
            row.try_get::<Option<String>, _>(name)
                .map_err(|e| database_error(format!("failed to read {name}: {e}")))
        };
        let get_i64 = |name: &str| -> Result<i64, CodypendentError> {
            row.try_get::<i64, _>(name)
                .map_err(|e| database_error(format!("failed to read {name}: {e}")))
        };
        let get_opt_i64 = |name: &str| -> Result<Option<i64>, CodypendentError> {
            row.try_get::<Option<i64>, _>(name)
                .map_err(|e| database_error(format!("failed to read {name}: {e}")))
        };

        let owner_raw = get_i64("owner_uid")?;
        let owner_uid = u32::try_from(owner_raw).map_err(|_| {
            refuse(
                "automation.owner-unresolvable",
                "the binding's owner uid is not a valid local uid",
            )
        })?;

        let workflow_id: WorkflowId = get_string("workflow_id")?
            .parse()
            .map_err(|e| database_error(format!("invalid workflow_id: {e}")))?;
        let repository_id: RepositoryId = get_string("repository_id")?
            .parse()
            .map_err(|e| database_error(format!("invalid repository_id: {e}")))?;

        let filters: TriggerFilters = serde_json::from_str(&get_string("filters_json")?)
            .map_err(|e| database_error(format!("failed to parse filters_json: {e}")))?;

        // `identity_fields` is not projected into its own column, so it is read
        // back out of the stored invocation policy. A policy that will not
        // deserialize is a refusal, not a default.
        let invocation: codypendent_protocol::InvocationPolicy =
            serde_json::from_str(&get_string("invocation_json")?)
                .map_err(|e| database_error(format!("failed to parse invocation_json: {e}")))?;

        let retry = TriggerRetryPolicy {
            max_attempts: u32::try_from(get_i64("retry_max_attempts")?).unwrap_or(0),
            initial_delay_seconds: u64::try_from(get_i64("retry_initial_delay_seconds")?)
                .unwrap_or(30),
            backoff_multiplier: u32::try_from(get_i64("retry_backoff_multiplier")?).unwrap_or(2),
            max_delay_seconds: get_opt_i64("retry_max_delay_seconds")?
                .and_then(|v| u64::try_from(v).ok()),
        };

        Ok(Self {
            id: get_string("id")?,
            owner_uid,
            source_type: get_string("source_type")?,
            cron_expression: get_opt("cron_expression")?,
            cron_timezone: get_opt("cron_timezone")?,
            one_time_at: get_opt("one_time_at")?,
            next_fire_at: get_opt("next_fire_at")?,
            last_fire_at: get_opt("last_fire_at")?,
            workflow_id,
            workflow_version: get_string("workflow_version")?,
            repository_id,
            repository_path: get_string("repository_path")?,
            filters,
            dedup_window_seconds: get_i64("dedup_window_seconds")?,
            concurrency_text: get_string("concurrency")?,
            missed_run_text: get_string("missed_run")?,
            missed_run_max_occurrences: get_opt_i64("missed_run_max_occurrences")?,
            retry,
            approval_mode_text: get_string("approval_mode")?,
            approval_receipt: get_opt("approval_receipt")?,
            budget: [
                get_opt_i64("budget_wall_time_seconds")?,
                get_opt_i64("budget_tool_calls")?,
                get_opt_i64("budget_tokens")?,
                get_opt_i64("budget_cost_micros")?,
            ],
            identity_fields: invocation.deduplication.identity_fields,
        })
    }

    /// Parse the TEXT policy columns, refusing every value this daemon does not
    /// recognize. A newer daemon may have written a variant this one has never
    /// heard of; running the binding with a *guessed* policy is strictly worse
    /// than not running it.
    pub fn policies(&self) -> Result<BindingPolicies, CodypendentError> {
        let concurrency = match self.concurrency_text.as_str() {
            "allow" => ConcurrencyPolicy::Allow,
            "skip" => ConcurrencyPolicy::Skip,
            "queue" => ConcurrencyPolicy::Queue,
            "replace" => ConcurrencyPolicy::Replace,
            other => {
                return Err(refuse(
                    "automation.unknown-policy",
                    format!("unrecognized concurrency policy '{other}'"),
                ))
            }
        };
        let missed_run = match self.missed_run_text.as_str() {
            "skip" => MissedRunPolicy::Skip,
            "run_once" => MissedRunPolicy::RunOnce,
            "catch_up" => {
                // The bound is the whole point of `catch_up`; without it the
                // policy is indistinguishable from unbounded replay.
                let max = self
                    .missed_run_max_occurrences
                    .and_then(|v| u32::try_from(v).ok());
                match max {
                    Some(max_occurrences) if max_occurrences > 0 => {
                        MissedRunPolicy::CatchUp { max_occurrences }
                    }
                    _ => {
                        return Err(refuse(
                            "automation.unknown-policy",
                            "catch_up missed-run policy has no usable max_occurrences bound",
                        ))
                    }
                }
            }
            other => {
                return Err(refuse(
                    "automation.unknown-policy",
                    format!("unrecognized missed_run policy '{other}'"),
                ))
            }
        };
        let approval_mode = match (self.approval_mode_text.as_str(), &self.approval_receipt) {
            ("inherit", _) => AutomationApprovalMode::Inherit,
            ("always_require", _) => AutomationApprovalMode::AlwaysRequire,
            ("policy_driven", _) => AutomationApprovalMode::PolicyDriven,
            ("preapproved", Some(receipt)) => AutomationApprovalMode::Preapproved {
                approval_receipt: receipt.clone(),
            },
            ("preapproved", None) => {
                return Err(refuse(
                    "automation.unknown-policy",
                    "preapproved approval mode has no receipt",
                ))
            }
            (other, _) => {
                return Err(refuse(
                    "automation.unknown-policy",
                    format!("unrecognized approval mode '{other}'"),
                ))
            }
        };
        Ok(BindingPolicies {
            concurrency,
            missed_run,
            approval_mode,
        })
    }

    /// The binding's declared ceiling as something the workflow-start seam can
    /// actually carry into the run, or the dotted code to refuse under.
    ///
    /// `Ok(None)` = the binding declares no ceiling. `Ok(Some(_))` = every
    /// dimension it declares is one the run's budget envelope enforces (wall-time
    /// and cost), so the firing may proceed with the ceiling attached.
    /// `Err("automation.budget-unenforceable")` = it declares a dimension nothing
    /// downstream charges — a tool-call cap (the workflow envelope has no
    /// tool-call dimension; only a per-role profile slice does, and a binding
    /// cannot reach one) or a token cap (tokens are recorded, never charged) — so
    /// dispatching would drop it silently.
    ///
    /// The four `budget_*` columns are the authority read here rather than
    /// `invocation_json`: `crate::automation` writes both from ONE
    /// `ProjectedInvocation` on create and on update, so they cannot diverge, and
    /// the columns are what `migrations/0044_automation.sql` designates.
    ///
    /// A stored value that is not a positive count refuses too. The schema's
    /// CHECKs make it unreachable through the create/update path, and a `0` read
    /// as "unset" would mean an operator's ceiling silently disappeared while a
    /// `0` honoured as a ceiling would mean "no budget at all".
    fn carryable_budget_ceiling(&self) -> Result<Option<WorkflowRunCeiling>, &'static str> {
        const UNENFORCEABLE: &str = "automation.budget-unenforceable";
        let [wall_time, tool_calls, tokens, cost_micros] = self.budget;
        if tool_calls.is_some() || tokens.is_some() {
            return Err(UNENFORCEABLE);
        }
        let positive = |value: Option<i64>| -> Result<Option<u64>, &'static str> {
            match value {
                None => Ok(None),
                Some(raw) if raw > 0 => u64::try_from(raw).map(Some).map_err(|_| UNENFORCEABLE),
                Some(_) => Err(UNENFORCEABLE),
            }
        };
        let ceiling = WorkflowRunCeiling {
            max_wall_time_seconds: positive(wall_time)?,
            max_cost_micros: positive(cost_micros)?,
        };
        Ok(if ceiling.declares_any() {
            Some(ceiling)
        } else {
            None
        })
    }
}

// ---------------------------------------------------------------------------
// The schedule decision (pure, so it is unit-testable without a database)
// ---------------------------------------------------------------------------

/// What a tick should do with one due binding.
// `CodypendentError` is `PartialEq` but not `Eq` (it is a wire type carrying a
// human message), so this enum can only be `PartialEq`.
#[derive(Debug, Clone, PartialEq)]
pub enum ScheduleDecision {
    /// Fire `occurrence`, then re-arm `next_fire_at` to `next` (`None` clears
    /// the column — a one-time binding that has fired is no longer due, which is
    /// the clearing path `validate_and_project_source` never had).
    Fire {
        occurrence: DateTime<Utc>,
        next: Option<DateTime<Utc>>,
    },
    /// Do not fire; only re-arm. This is `missed_run = skip` catching up on a
    /// backlog: `last_fire_at` is left untouched because nothing fired.
    Advance { next: Option<DateTime<Utc>> },
    /// Refuse: leave the row exactly as it is and dispatch nothing.
    Refuse(CodypendentError),
}

/// Decide what to do with a binding whose `next_fire_at` is `due`.
///
/// Pure: no database, no clock of its own. `now` and the grace window are
/// parameters so a test can pin a year-long backlog without waiting for one.
pub fn decide_schedule(
    binding: &BindingRecord,
    policies: &BindingPolicies,
    due: DateTime<Utc>,
    now: DateTime<Utc>,
    grace_seconds: i64,
) -> ScheduleDecision {
    let is_cron = binding.source_type == "cron";
    let is_one_time = binding.source_type == "one_time";
    if !is_cron && !is_one_time {
        // A `next_fire_at` on an event-sourced binding is a schema violation, not
        // a schedule. Refuse rather than invent an occurrence for it.
        return ScheduleDecision::Refuse(refuse(
            "automation.not-scheduled",
            format!(
                "binding source '{}' has no schedule but carries next_fire_at",
                binding.source_type
            ),
        ));
    }

    let missed = now.signed_duration_since(due) > TimeDelta::seconds(grace_seconds);

    if is_one_time {
        if missed && matches!(policies.missed_run, MissedRunPolicy::Skip) {
            // The instant passed while the daemon was down and the operator asked
            // for `skip`: clear the column so it is not permanently due, and fire
            // nothing.
            return ScheduleDecision::Advance { next: None };
        }
        if missed && matches!(policies.missed_run, MissedRunPolicy::Unknown) {
            return ScheduleDecision::Refuse(refuse(
                "automation.unknown-policy",
                "unrecognized missed_run policy",
            ));
        }
        return ScheduleDecision::Fire {
            occurrence: due,
            next: None,
        };
    }

    let (Some(expression), Some(timezone)) = (&binding.cron_expression, &binding.cron_timezone)
    else {
        return ScheduleDecision::Refuse(refuse(
            "automation.not-scheduled",
            "cron binding is missing its expression or timezone",
        ));
    };

    let next_after = |from: DateTime<Utc>| next_cron_occurrence_after(expression, timezone, from);

    if !missed {
        return match next_after(due) {
            Ok(next) => ScheduleDecision::Fire {
                occurrence: due,
                next: Some(next),
            },
            Err(error) => ScheduleDecision::Refuse(error),
        };
    }

    // Enumerate the missed occurrences in (due, now], bounded. `due` itself is
    // the first missed occurrence.
    let mut occurrences = vec![due];
    let mut cursor = due;
    loop {
        if occurrences.len() > MAX_OCCURRENCE_SCAN {
            return ScheduleDecision::Refuse(refuse(
                "automation.occurrence-backlog-too-large",
                format!(
                    "more than {MAX_OCCURRENCE_SCAN} missed occurrences since {due}; \
                     refusing rather than replaying an unbounded backlog"
                ),
            ));
        }
        match next_after(cursor) {
            Ok(next) if next <= now => {
                occurrences.push(next);
                cursor = next;
            }
            Ok(_) => break,
            Err(error) => return ScheduleDecision::Refuse(error),
        }
    }

    match policies.missed_run {
        MissedRunPolicy::Skip => match next_after(now) {
            Ok(next) => ScheduleDecision::Advance { next: Some(next) },
            Err(error) => ScheduleDecision::Refuse(error),
        },
        MissedRunPolicy::RunOnce => {
            // Fire the MOST RECENT missed occurrence once, then resume the normal
            // schedule from now. Firing the oldest would run stale work.
            let occurrence = *occurrences.last().unwrap_or(&due);
            match next_after(now) {
                Ok(next) => ScheduleDecision::Fire {
                    occurrence,
                    next: Some(next),
                },
                Err(error) => ScheduleDecision::Refuse(error),
            }
        }
        MissedRunPolicy::CatchUp { max_occurrences } => {
            // Retain only the most recent `max_occurrences`, then fire the oldest
            // of what is retained; the next tick fires the one after it. Because
            // the retained window is recomputed from the (now advanced)
            // `next_fire_at` each tick and never grows, the backlog replayed is
            // bounded by `max_occurrences` in total.
            let keep = usize::try_from(max_occurrences).unwrap_or(1).max(1);
            let start = occurrences.len().saturating_sub(keep);
            let occurrence = occurrences[start];
            match next_after(occurrence) {
                Ok(next) => ScheduleDecision::Fire {
                    occurrence,
                    next: Some(next),
                },
                Err(error) => ScheduleDecision::Refuse(error),
            }
        }
        // Fail closed. `validate_and_project_invocation` rejects an unknown
        // policy at create time, so reaching here means a downgrade or a hand-
        // edited row — exactly when guessing is most dangerous.
        MissedRunPolicy::Unknown | _ => ScheduleDecision::Refuse(refuse(
            "automation.unknown-policy",
            "unrecognized missed_run policy",
        )),
    }
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

/// Whether `event` satisfies `filters`.
///
/// Fail-closed on dimensions this daemon cannot evaluate: a binding that filters
/// on `paths` or `labels` — for which the normalizer produces no field — does
/// NOT match, because the alternative is running an automation whose author
/// narrowed it and getting the wide behaviour anyway.
#[must_use]
pub fn event_matches_filters(filters: &TriggerFilters, event: &TriggerEvent) -> bool {
    fn matches_any(candidates: &[String], value: Option<&str>) -> bool {
        match value {
            Some(value) => candidates.iter().any(|c| c == value),
            None => false,
        }
    }

    if !filters.branches.is_empty() {
        // `git_ref` arrives fully qualified (`refs/heads/main`); accept either
        // the qualified ref or the bare branch name the operator typed.
        let branch = event
            .field("branch")
            .or_else(|| event.field("git_ref"))
            .map(|value| value.strip_prefix("refs/heads/").unwrap_or(value));
        if !matches_any(&filters.branches, branch) {
            return false;
        }
    }
    if !filters.actors.is_empty() && !matches_any(&filters.actors, event.field("actor")) {
        return false;
    }
    if !filters.paths.is_empty() {
        // No normalized field carries changed paths. Refuse to widen.
        return false;
    }
    if !filters.labels.is_empty() {
        return false;
    }
    for (key, expected) in &filters.metadata {
        if event.field(key) != Some(expected.as_str()) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// The scheduler
// ---------------------------------------------------------------------------

/// What one [`AutomationScheduler::tick`] did. Counts are honest: a firing is
/// counted only once a run id came back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickReport {
    pub examined: usize,
    pub claimed: usize,
    pub started: usize,
    pub skipped: usize,
    pub refused: usize,
    pub retried: usize,
}

/// The durable automation scheduler and trigger dispatcher.
#[derive(Clone)]
pub struct AutomationScheduler {
    pool: SqlitePool,
    holder: DaemonInstanceId,
    starter: Arc<dyn WorkflowStarter>,
    environment: Arc<dyn AutomationEnvironment>,
}

/// The outcome the dispatcher recorded, for a caller that wants to log it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    Started,
    Skipped,
    Filtered,
    Denied,
    RetryScheduled,
    Failed,
}

impl DispatchOutcome {
    const fn column(self) -> &'static str {
        match self {
            DispatchOutcome::Started => "started",
            DispatchOutcome::Skipped => "skipped_concurrency",
            DispatchOutcome::Filtered => "filtered",
            DispatchOutcome::Denied => "denied",
            DispatchOutcome::RetryScheduled | DispatchOutcome::Failed => "error",
        }
    }
}

/// One row of the retry/orphan sweep: `(receipt id, binding id, occurrence_at,
/// delivery_id, dedup_key, highest attempt so far)`.
type PendingReceiptRow = (String, String, Option<String>, Option<String>, String, i64);

/// A reservation the dispatcher owns.
#[derive(Debug, Clone)]
struct Reservation {
    receipt_id: String,
    dedup_key: String,
    occurrence_at: Option<String>,
    delivery_id: Option<String>,
    idempotency_key: String,
    attempt: i64,
}

impl AutomationScheduler {
    #[must_use]
    pub fn new(
        pool: SqlitePool,
        holder: DaemonInstanceId,
        starter: Arc<dyn WorkflowStarter>,
        environment: Arc<dyn AutomationEnvironment>,
    ) -> Self {
        Self {
            pool,
            holder,
            starter,
            environment,
        }
    }

    /// One scheduler pass: recover interrupted work, claim every due binding,
    /// and re-drive whatever is owed a retry.
    ///
    /// Never returns an error: a scheduler that can kill the daemon's startup or
    /// stop ticking on one bad row is worse than one that logs and continues, and
    /// this mirrors `retrieval::spawn_index_maintenance`'s warn-and-swallow loop.
    pub async fn tick(&self, now: DateTime<Utc>) -> TickReport {
        let mut report = TickReport::default();

        if let Err(error) = self.close_interrupted_attempts(now).await {
            warn!(code = %error.code, reason = %error.message, "could not close interrupted automation attempts");
        }

        match self.claim_due(now).await {
            Ok((examined, reservations)) => {
                report.examined = examined;
                report.claimed = reservations.len();
                for (reservation, binding) in reservations {
                    match self
                        .dispatch(&reservation, &binding, &TriggerEvent::default(), now)
                        .await
                    {
                        DispatchOutcome::Started => report.started += 1,
                        DispatchOutcome::Skipped | DispatchOutcome::Filtered => {
                            report.skipped += 1;
                        }
                        DispatchOutcome::RetryScheduled => report.retried += 1,
                        DispatchOutcome::Denied | DispatchOutcome::Failed => report.refused += 1,
                    }
                }
            }
            Err(error) => {
                warn!(code = %error.code, reason = %error.message, "automation due-claim failed")
            }
        }

        match self.sweep_pending(now).await {
            Ok(count) => report.retried += count,
            Err(error) => {
                warn!(code = %error.code, reason = %error.message, "automation retry sweep failed")
            }
        }

        report
    }

    /// Spawn the production tick loop. Fire-and-forget, exactly like
    /// `retrieval::spawn_index_maintenance`: errors are warned and swallowed, and
    /// nothing here can ever be fatal to startup.
    pub fn spawn(self, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            info!(
                interval_seconds = interval.as_secs(),
                "automation scheduler started"
            );
            loop {
                let report = self.tick(Utc::now()).await;
                if report.claimed > 0 || report.retried > 0 {
                    debug!(
                        examined = report.examined,
                        claimed = report.claimed,
                        started = report.started,
                        skipped = report.skipped,
                        refused = report.refused,
                        retried = report.retried,
                        "automation tick"
                    );
                }
                tokio::time::sleep(interval).await;
            }
        })
    }

    // -- claiming ---------------------------------------------------------

    /// Select every due binding and try to claim each. Returns how many were
    /// examined and the reservations actually won.
    async fn claim_due(
        &self,
        now: DateTime<Utc>,
    ) -> Result<(usize, Vec<(Reservation, BindingRecord)>), CodypendentError> {
        // Exactly the shape `idx_automation_bindings_due` (a partial index on
        // next_fire_at WHERE enabled = 1 AND next_fire_at IS NOT NULL) serves.
        // All stored instants are UTC RFC3339, so the lexicographic comparison is
        // a chronological one.
        let due: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, next_fire_at FROM automation_bindings \
             WHERE enabled = 1 AND next_fire_at IS NOT NULL AND next_fire_at <= ? \
             ORDER BY next_fire_at LIMIT ?",
        )
        .bind(now.to_rfc3339())
        .bind(MAX_CLAIMS_PER_TICK)
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?;

        let examined = due.len();
        let mut claimed = Vec::new();
        for (binding_id, expected_next) in due {
            let Some(fence) = self.acquire_lease(&binding_id, now).await? else {
                // Another daemon instance holds an unexpired lease on this
                // binding. Not an error: it is the outer guard doing its job.
                continue;
            };
            match self
                .claim_one(&binding_id, &expected_next, fence, now)
                .await
            {
                Ok(Some(pair)) => claimed.push(pair),
                Ok(None) => {}
                Err(error) => {
                    warn!(binding = %binding_id, code = %error.code, reason = %error.message, "automation claim refused");
                }
            }
        }
        Ok((examined, claimed))
    }

    /// Take (or renew) this binding's durable lease, returning the fence to
    /// re-check inside the claim. `None` when a live lease is held elsewhere.
    ///
    /// The migration explicitly forbids reusing the in-process
    /// `DriveLockRegistry` here: it is a `HashMap` in one process and provides no
    /// exclusion across a restart or a second daemon instance.
    async fn acquire_lease(
        &self,
        binding_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<i64>, CodypendentError> {
        let expires = (now + TimeDelta::seconds(LEASE_TTL_SECONDS)).to_rfc3339();
        let row: Option<(i64,)> = sqlx::query_as(
            "INSERT INTO automation_leases (binding_id, holder, acquired_at, expires_at, fence) \
             VALUES (?, ?, ?, ?, 1) \
             ON CONFLICT (binding_id) DO UPDATE SET \
                holder = excluded.holder, \
                acquired_at = excluded.acquired_at, \
                expires_at = excluded.expires_at, \
                fence = automation_leases.fence + 1 \
             WHERE automation_leases.expires_at < ? OR automation_leases.holder = ? \
             RETURNING fence",
        )
        .bind(binding_id)
        .bind(self.holder.to_string())
        .bind(now.to_rfc3339())
        .bind(&expires)
        .bind(now.to_rfc3339())
        .bind(self.holder.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?;
        Ok(row.map(|(fence,)| fence))
    }

    /// The atomic claim. Everything that decides whether this binding may fire is
    /// re-read INSIDE the transaction — `delete_binding` is a plain DELETE with no
    /// lease check, so a value read before the transaction proves nothing.
    async fn claim_one(
        &self,
        binding_id: &str,
        expected_next: &str,
        fence: i64,
        now: DateTime<Utc>,
    ) -> Result<Option<(Reservation, BindingRecord)>, CodypendentError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(database_error)?;

        // Fence first: if another instance took the lease between our acquire and
        // this transaction, its fence is higher and we must write nothing.
        let live_fence: Option<(i64,)> = sqlx::query_as(
            "SELECT fence FROM automation_leases WHERE binding_id = ? AND holder = ?",
        )
        .bind(binding_id)
        .bind(self.holder.to_string())
        .fetch_optional(&mut *tx)
        .await
        .map_err(database_error)?;
        if live_fence.map(|(f,)| f) != Some(fence) {
            return Ok(None);
        }

        // Re-read under the write lock. `enabled = 1` and the exact
        // `next_fire_at` are part of the predicate, never a pre-read.
        let row = sqlx::query(&format!(
            "SELECT {BINDING_COLUMNS} FROM automation_bindings \
             WHERE id = ? AND enabled = 1 AND next_fire_at = ?"
        ))
        .bind(binding_id)
        .bind(expected_next)
        .fetch_optional(&mut *tx)
        .await
        .map_err(database_error)?;
        let Some(row) = row else {
            // Deleted, disabled, or already advanced by a concurrent tick.
            return Ok(None);
        };
        let binding = BindingRecord::from_row(&row)?;
        let policies = binding.policies()?;

        let due = DateTime::parse_from_rfc3339(expected_next)
            .map_err(|e| database_error(format!("invalid next_fire_at '{expected_next}': {e}")))?
            .with_timezone(&Utc);

        let decision = decide_schedule(&binding, &policies, due, now, MISSED_RUN_GRACE_SECONDS);
        let (occurrence, next) = match decision {
            ScheduleDecision::Fire { occurrence, next } => (Some(occurrence), next),
            ScheduleDecision::Advance { next } => (None, next),
            ScheduleDecision::Refuse(error) => {
                // Leave the row untouched: a refusal must not silently consume the
                // occurrence, and must not advance past work the operator may
                // still want after fixing the binding.
                return Err(error);
            }
        };

        // The compare-and-swap. `updated_at` is deliberately NOT touched: it
        // orders the client's keyset pagination, and churning it on every firing
        // would reshuffle every open listing.
        let advanced = sqlx::query(
            "UPDATE automation_bindings \
             SET next_fire_at = ?, last_fire_at = COALESCE(?, last_fire_at) \
             WHERE id = ? AND enabled = 1 AND next_fire_at = ?",
        )
        .bind(next.map(|n| n.to_rfc3339()))
        .bind(occurrence.map(|o| o.to_rfc3339()))
        .bind(binding_id)
        .bind(expected_next)
        .execute(&mut *tx)
        .await
        .map_err(database_error)?
        .rows_affected();
        if advanced == 0 {
            return Ok(None);
        }

        let Some(occurrence) = occurrence else {
            // `missed_run = skip`: re-armed, nothing fired, no receipt and no
            // attempt — because no execution was attempted.
            tx.commit().await.map_err(database_error)?;
            return Ok(None);
        };

        let dedup_key = format!("occurrence:{}", occurrence.to_rfc3339());
        let reservation = insert_receipt(
            &mut tx,
            &binding,
            &dedup_key,
            Some(occurrence.to_rfc3339()),
            None,
            None,
            now,
        )
        .await?;
        tx.commit().await.map_err(database_error)?;

        Ok(reservation.map(|reservation| (reservation, binding)))
    }

    // -- dispatch ---------------------------------------------------------

    /// Drive one reserved receipt: gate it against the binding's own policy, then
    /// start the workflow. Always finishes the attempt row with the outcome that
    /// actually happened.
    async fn dispatch(
        &self,
        reservation: &Reservation,
        binding: &BindingRecord,
        event: &TriggerEvent,
        now: DateTime<Utc>,
    ) -> DispatchOutcome {
        match self.dispatch_inner(reservation, binding, event, now).await {
            Ok(outcome) => outcome,
            Err(error) => {
                warn!(
                    binding = %binding.id,
                    receipt = %reservation.receipt_id,
                    code = %error.code,
                    "automation dispatch failed"
                );
                let _ = self
                    .finish(
                        reservation,
                        DispatchOutcome::Failed,
                        Some(&error.code),
                        None,
                        "failed",
                        now,
                    )
                    .await;
                DispatchOutcome::Failed
            }
        }
    }

    async fn dispatch_inner(
        &self,
        reservation: &Reservation,
        binding: &BindingRecord,
        event: &TriggerEvent,
        now: DateTime<Utc>,
    ) -> Result<DispatchOutcome, CodypendentError> {
        let policies = match binding.policies() {
            Ok(policies) => policies,
            Err(error) => {
                return self
                    .deny(reservation, &error.code, now)
                    .await
                    .map(|()| DispatchOutcome::Denied)
            }
        };

        // 1. Owner. The run executes as the binding's owner or not at all: there
        // is deliberately no fallback to `state.daemon_uid`, which would silently
        // escalate an orphaned binding to the daemon's own authority.
        if !self.environment.owner_is_resolvable(binding.owner_uid) {
            return self
                .deny(reservation, "automation.owner-unresolvable", now)
                .await
                .map(|()| DispatchOutcome::Denied);
        }

        // 2. Policy, split by what this daemon can genuinely carry into the run.
        //
        // BUDGET. `StartWorkflowRequest::budget_ceiling` carries a wall-time and a
        // cost ceiling across the start seam, and the seam tightens the run's
        // STORED manifest with it — so the ceiling is charged by `BudgetLimits`
        // for every node and survives a restart, because the envelope in force is
        // recompiled from that stored manifest. Those are the only two dimensions
        // the workflow envelope has: a binding declaring a tool-call or token cap
        // still refuses here rather than run with it dropped.
        let ceiling = match binding.carryable_budget_ceiling() {
            Ok(ceiling) => ceiling,
            Err(code) => {
                return self
                    .deny(reservation, code, now)
                    .await
                    .map(|()| DispatchOutcome::Denied)
            }
        };
        // APPROVAL. Still refused, and deliberately not given a field to travel
        // in. Nothing downstream can honour a run-level approval floor: an agent
        // node never consults `CompiledNode::approval` at all (only the
        // `repository.test` tool node does), there is no run-scoped "require
        // approval for every effect" switch anywhere in the runtime, and no path
        // consumes a pre-issued approval receipt. Carrying an approval mode would
        // make `always_require` look enforced while agent effects executed
        // unapproved — strictly worse than not firing.
        if !matches!(policies.approval_mode, AutomationApprovalMode::Inherit) {
            return self
                .deny(reservation, "automation.approval-unenforceable", now)
                .await
                .map(|()| DispatchOutcome::Denied);
        }

        // 3. Filters (event sources only; a schedule has no event to filter).
        if !event.event_type.is_empty() && !event_matches_filters(&binding.filters, event) {
            self.finish(
                reservation,
                DispatchOutcome::Filtered,
                None,
                None,
                "skipped",
                now,
            )
            .await?;
            return Ok(DispatchOutcome::Filtered);
        }

        // 4. Concurrency.
        match policies.concurrency {
            ConcurrencyPolicy::Allow => {}
            ConcurrencyPolicy::Skip => {
                if self.has_live_run(&binding.id).await? {
                    self.finish(
                        reservation,
                        DispatchOutcome::Skipped,
                        None,
                        None,
                        "skipped",
                        now,
                    )
                    .await?;
                    return Ok(DispatchOutcome::Skipped);
                }
            }
            // `queue` needs a durable pending-firing queue and `replace` needs to
            // cancel the in-flight run through `WorkflowLifecycle`; neither seam
            // is wired here. Denying is honest; treating them as `allow` would
            // silently double-run an automation whose author asked for exactly the
            // opposite.
            ConcurrencyPolicy::Queue | ConcurrencyPolicy::Replace => {
                return self
                    .deny(reservation, "automation.concurrency-unsupported", now)
                    .await
                    .map(|()| DispatchOutcome::Denied)
            }
            ConcurrencyPolicy::Unknown | _ => {
                return self
                    .deny(reservation, "automation.unknown-policy", now)
                    .await
                    .map(|()| DispatchOutcome::Denied)
            }
        }

        // 5. The checkout must still be the one the binding was created against.
        // `RepositoryId` is a one-way hash of the canonical path, so a moved or
        // renamed checkout is detected by re-deriving it.
        match self
            .environment
            .repository_id_for(Path::new(&binding.repository_path))
        {
            Some(derived) if derived == binding.repository_id => {}
            _ => {
                return self
                    .deny(reservation, "automation.repository-moved", now)
                    .await
                    .map(|()| DispatchOutcome::Denied)
            }
        }

        // 6. The workflow must resolve.
        let workflow_name = match self.environment.resolve_workflow(
            binding.workflow_id,
            &binding.workflow_version,
            Path::new(&binding.repository_path),
        ) {
            Ok(name) => name,
            Err(error) => {
                return self
                    .deny(reservation, &error.code, now)
                    .await
                    .map(|()| DispatchOutcome::Denied)
            }
        };

        // 7. Start. The idempotency key is the receipt's, so a crash between here
        // and the receipt update resolves to the SAME run on re-drive.
        let request = StartWorkflowRequest {
            expected_definition_digest: None,
            manifest: String::new(),
            workflow_id: Some(workflow_name),
            inputs: automation_inputs(binding, reservation, event),
            idempotency_key: reservation.idempotency_key.clone(),
            repository: Some(binding.repository_path.clone()),
            owner_uid: binding.owner_uid,
            client_id: ClientId::new(),
            // The binding row's ceiling — never the payload's — tightening the
            // run's own envelope.
            budget_ceiling: ceiling,
        };

        match self.starter.start(request).await {
            Ok(run_id) => {
                self.finish(
                    reservation,
                    DispatchOutcome::Started,
                    None,
                    Some(&run_id),
                    "dispatched",
                    now,
                )
                .await?;
                Ok(DispatchOutcome::Started)
            }
            Err(error) => {
                let next_attempt = reservation.attempt + 1;
                let retryable =
                    error.retryable && next_attempt <= i64::from(binding.retry.max_attempts);
                if retryable {
                    let delay = retry_delay_seconds(&binding.retry, reservation.attempt);
                    let next_retry_at = (now + TimeDelta::seconds(delay)).to_rfc3339();
                    // The attempt stays OPEN (`finished_at` NULL) with
                    // `next_retry_at` set: that is precisely the row shape
                    // `idx_automation_attempts_retry` indexes, and the sweep
                    // closes it when it re-drives.
                    sqlx::query(
                        "UPDATE automation_attempts SET outcome = 'error', error_code = ?, \
                         next_retry_at = ? WHERE receipt_id = ? AND attempt = ?",
                    )
                    .bind(&error.code)
                    .bind(&next_retry_at)
                    .bind(&reservation.receipt_id)
                    .bind(reservation.attempt)
                    .execute(&self.pool)
                    .await
                    .map_err(database_error)?;
                    self.touch_receipt(reservation, "reserved", Some(&error.code), None, now)
                        .await?;
                    Ok(DispatchOutcome::RetryScheduled)
                } else {
                    self.finish(
                        reservation,
                        DispatchOutcome::Failed,
                        Some(&error.code),
                        None,
                        "failed",
                        now,
                    )
                    .await?;
                    Ok(DispatchOutcome::Failed)
                }
            }
        }
    }

    async fn deny(
        &self,
        reservation: &Reservation,
        code: &str,
        now: DateTime<Utc>,
    ) -> Result<(), CodypendentError> {
        self.finish(
            reservation,
            DispatchOutcome::Denied,
            Some(code),
            None,
            "failed",
            now,
        )
        .await
    }

    /// Close the attempt and settle the receipt in one transaction, so a receipt
    /// can never claim an outcome its attempt does not record.
    async fn finish(
        &self,
        reservation: &Reservation,
        outcome: DispatchOutcome,
        error_code: Option<&str>,
        run_id: Option<&str>,
        receipt_state: &str,
        now: DateTime<Utc>,
    ) -> Result<(), CodypendentError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(database_error)?;
        sqlx::query(
            "UPDATE automation_attempts SET finished_at = ?, outcome = ?, error_code = ?, \
             next_retry_at = NULL WHERE receipt_id = ? AND attempt = ?",
        )
        .bind(now.to_rfc3339())
        .bind(outcome.column())
        .bind(error_code)
        .bind(&reservation.receipt_id)
        .bind(reservation.attempt)
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;

        // `workflow_run_id` is write-once: the `IS NULL` predicate means a
        // re-drive that resolved to the already-recorded run cannot overwrite it.
        sqlx::query(
            "UPDATE automation_receipts SET state = ?, \
             workflow_run_id = COALESCE(workflow_run_id, ?), last_error = ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(receipt_state)
        .bind(run_id)
        .bind(error_code)
        .bind(now.to_rfc3339())
        .bind(&reservation.receipt_id)
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
        tx.commit().await.map_err(database_error)
    }

    async fn touch_receipt(
        &self,
        reservation: &Reservation,
        state: &str,
        error_code: Option<&str>,
        run_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), CodypendentError> {
        sqlx::query(
            "UPDATE automation_receipts SET state = ?, \
             workflow_run_id = COALESCE(workflow_run_id, ?), last_error = ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(state)
        .bind(run_id)
        .bind(error_code)
        .bind(now.to_rfc3339())
        .bind(&reservation.receipt_id)
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        Ok(())
    }

    /// Whether this binding already has a non-terminal run. Joined through
    /// `automation_receipts` because `workflow_runs` has no binding column.
    async fn has_live_run(&self, binding_id: &str) -> Result<bool, CodypendentError> {
        let live: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM automation_receipts r \
             JOIN workflow_runs w ON w.id = r.workflow_run_id \
             WHERE r.binding_id = ? AND r.workflow_run_id IS NOT NULL \
               AND w.state IN {NON_TERMINAL_RUN_STATES}"
        ))
        .bind(binding_id)
        .fetch_one(&self.pool)
        .await
        .map_err(database_error)?;
        Ok(live > 0)
    }

    // -- recovery ---------------------------------------------------------

    /// Close attempts a crash left open. Without this their receipts would sit in
    /// `reserved` forever: the retry index only finds rows with `next_retry_at`.
    async fn close_interrupted_attempts(
        &self,
        now: DateTime<Utc>,
    ) -> Result<u64, CodypendentError> {
        let cutoff = (now - TimeDelta::seconds(STALE_ATTEMPT_SECONDS)).to_rfc3339();
        let closed = sqlx::query(
            "UPDATE automation_attempts SET finished_at = ?, \
             outcome = COALESCE(outcome, 'error'), \
             error_code = COALESCE(error_code, 'automation.attempt-interrupted') \
             WHERE finished_at IS NULL AND next_retry_at IS NULL AND started_at < ?",
        )
        .bind(now.to_rfc3339())
        .bind(cutoff)
        .execute(&self.pool)
        .await
        .map_err(database_error)?
        .rows_affected();
        Ok(closed)
    }

    /// Re-drive every receipt that is owed one: an attempt whose `next_retry_at`
    /// has come, and any reservation left `reserved` with nothing in flight (the
    /// crash-between-reserve-and-dispatch case). Returns how many were re-driven.
    async fn sweep_pending(&self, now: DateTime<Utc>) -> Result<usize, CodypendentError> {
        let stamp = now.to_rfc3339();
        let orphan_cutoff = (now - TimeDelta::seconds(ORPHAN_RECEIPT_SECONDS)).to_rfc3339();
        let rows: Vec<PendingReceiptRow> = sqlx::query_as(
            "SELECT r.id, r.binding_id, r.occurrence_at, r.delivery_id, \
                        r.dedup_key, COALESCE(MAX(a.attempt), 0) \
                 FROM automation_receipts r \
                 LEFT JOIN automation_attempts a ON a.receipt_id = r.id \
                 WHERE r.state = 'reserved' \
                 GROUP BY r.id \
                 HAVING ( \
                    SUM(CASE WHEN a.finished_at IS NULL AND a.next_retry_at IS NOT NULL \
                                  AND a.next_retry_at <= ? THEN 1 ELSE 0 END) > 0 \
                 ) OR ( \
                    SUM(CASE WHEN a.finished_at IS NULL THEN 1 ELSE 0 END) = 0 \
                    AND r.updated_at <= ? \
                 ) \
                 ORDER BY r.reserved_at LIMIT ?",
        )
        .bind(&stamp)
        .bind(&orphan_cutoff)
        .bind(MAX_CLAIMS_PER_TICK)
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?;

        let mut driven = 0usize;
        for (receipt_id, binding_id, occurrence_at, delivery_id, dedup_key, last_attempt) in rows {
            // Close whatever is still open on this receipt before opening the
            // next attempt: `UNIQUE (receipt_id, attempt)` demands a fresh number.
            sqlx::query(
                "UPDATE automation_attempts SET finished_at = ?, \
                 outcome = COALESCE(outcome, 'error'), \
                 error_code = COALESCE(error_code, 'automation.attempt-interrupted'), \
                 next_retry_at = NULL \
                 WHERE receipt_id = ? AND finished_at IS NULL",
            )
            .bind(&stamp)
            .bind(&receipt_id)
            .execute(&self.pool)
            .await
            .map_err(database_error)?;

            // The binding must still exist and still be enabled. A receipt whose
            // binding was disabled or deleted is ABANDONED, never fired.
            let row = sqlx::query(&format!(
                "SELECT {BINDING_COLUMNS} FROM automation_bindings WHERE id = ? AND enabled = 1"
            ))
            .bind(&binding_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(database_error)?;
            let Some(row) = row else {
                sqlx::query(
                    "UPDATE automation_receipts SET state = 'abandoned', \
                     last_error = 'automation.binding-unavailable', updated_at = ? WHERE id = ?",
                )
                .bind(&stamp)
                .bind(&receipt_id)
                .execute(&self.pool)
                .await
                .map_err(database_error)?;
                continue;
            };
            let binding = BindingRecord::from_row(&row)?;

            let attempt = last_attempt + 1;
            let idempotency_key = idempotency_key_for(&binding_id, &dedup_key);
            open_attempt(&self.pool, &receipt_id, attempt, now).await?;
            let reservation = Reservation {
                receipt_id,
                dedup_key,
                occurrence_at,
                delivery_id,
                idempotency_key,
                attempt,
            };
            self.dispatch(&reservation, &binding, &TriggerEvent::default(), now)
                .await;
            driven += 1;
        }
        Ok(driven)
    }

    // -- event-sourced dispatch (the webhook path) ------------------------

    /// Fan one verified, normalized delivery out to every enabled binding on
    /// `endpoint_id`.
    ///
    /// Never returns an error for a binding-level policy decision: this runs
    /// inside webhook ingestion, whose sink error would fail the whole delivery
    /// and make the provider retry a delivery that was correctly filtered.
    pub async fn dispatch_endpoint_event(
        &self,
        endpoint_id: &str,
        event: &TriggerEvent,
        now: DateTime<Utc>,
    ) -> usize {
        let rows = sqlx::query(&format!(
            "SELECT {BINDING_COLUMNS} FROM automation_bindings \
             WHERE endpoint_id = ? AND enabled = 1"
        ))
        .bind(endpoint_id)
        .fetch_all(&self.pool)
        .await;
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) => {
                warn!(%error, "automation endpoint lookup failed");
                return 0;
            }
        };

        let mut dispatched = 0usize;
        for row in &rows {
            let binding = match BindingRecord::from_row(row) {
                Ok(binding) => binding,
                Err(error) => {
                    warn!(code = %error.code, "skipping unreadable automation binding");
                    continue;
                }
            };
            match self.reserve_event(&binding, event, now).await {
                Ok(Some(reservation)) => {
                    self.dispatch(&reservation, &binding, event, now).await;
                    dispatched += 1;
                }
                // Already reserved: this delivery has been seen for this binding.
                Ok(None) => {}
                Err(error) => {
                    warn!(binding = %binding.id, code = %error.code, "automation reservation refused");
                }
            }
        }
        dispatched
    }

    /// Reserve this delivery for this binding, deriving the dedup key
    /// **server-side** from the fields the binding's policy names — never from
    /// anything the payload chose.
    async fn reserve_event(
        &self,
        binding: &BindingRecord,
        event: &TriggerEvent,
        now: DateTime<Utc>,
    ) -> Result<Option<Reservation>, CodypendentError> {
        let dedup_key = derive_dedup_key(binding, event)?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(database_error)?;
        let reservation = insert_receipt(
            &mut tx,
            binding,
            &dedup_key,
            None,
            event.delivery_id.clone(),
            event.content_fingerprint.clone(),
            now,
        )
        .await?;
        tx.commit().await.map_err(database_error)?;
        Ok(reservation)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The workflow-store idempotency key for one reservation. Stable across
/// re-drives, which is what makes `deterministic_run_id("owner:{uid}:{key}")` the
/// third double-fire guard.
fn idempotency_key_for(binding_id: &str, dedup_key: &str) -> String {
    format!("automation:{binding_id}:{dedup_key}")
}

/// Derive the reservation identity for an event delivery.
///
/// With no `identity_fields` the provider's delivery id is the identity. With
/// them, every named field must be PRESENT: a missing one would silently produce
/// a different (and colliding) key, so it refuses instead.
fn derive_dedup_key(
    binding: &BindingRecord,
    event: &TriggerEvent,
) -> Result<String, CodypendentError> {
    if binding.identity_fields.is_empty() {
        return match &event.delivery_id {
            Some(delivery_id) => Ok(format!("delivery:{delivery_id}")),
            None => Err(refuse(
                "automation.dedup-identity-missing",
                "the event carries no delivery id and the binding names no identity fields",
            )),
        };
    }
    let mut parts = Vec::with_capacity(binding.identity_fields.len());
    for name in &binding.identity_fields {
        let Some(value) = event.fields.get(name) else {
            return Err(refuse(
                "automation.dedup-field-missing",
                format!("the event has no normalized field '{name}'"),
            ));
        };
        parts.push(format!("{name}={value}"));
    }
    Ok(format!("event:{}:{}", event.event_type, parts.join("&")))
}

/// The inputs the run receives. Trigger context only — a binding carries no
/// operator-supplied inputs, and the payload never selects owner, repository,
/// workflow or budget (the binding row is the sole authority for those).
fn automation_inputs(
    binding: &BindingRecord,
    reservation: &Reservation,
    event: &TriggerEvent,
) -> Value {
    let mut map = serde_json::Map::new();
    for (key, value) in &event.fields {
        map.insert(key.clone(), Value::String(value.clone()));
    }
    map.insert(
        "automation_binding_id".into(),
        Value::String(binding.id.clone()),
    );
    map.insert(
        "automation_source_type".into(),
        Value::String(binding.source_type.clone()),
    );
    map.insert(
        "automation_dedup_key".into(),
        Value::String(reservation.dedup_key.clone()),
    );
    if let Some(occurrence) = &reservation.occurrence_at {
        map.insert(
            "automation_occurrence_at".into(),
            Value::String(occurrence.clone()),
        );
    }
    if let Some(delivery) = &reservation.delivery_id {
        map.insert(
            "automation_delivery_id".into(),
            Value::String(delivery.clone()),
        );
    }
    if !event.event_type.is_empty() {
        map.insert(
            "automation_event_type".into(),
            Value::String(event.event_type.clone()),
        );
    }
    Value::Object(map)
}

/// The exponential backoff for trigger-level retries. Saturating throughout: a
/// large multiplier must not overflow into a negative delay.
fn retry_delay_seconds(retry: &TriggerRetryPolicy, attempt: i64) -> i64 {
    let base = i64::try_from(retry.initial_delay_seconds)
        .unwrap_or(30)
        .max(0);
    let multiplier = i64::from(retry.backoff_multiplier.max(1));
    let exponent = u32::try_from(attempt.saturating_sub(1).clamp(0, 16)).unwrap_or(0);
    let delay = base.saturating_mul(multiplier.saturating_pow(exponent));
    match retry.max_delay_seconds {
        Some(max) => delay.min(i64::try_from(max).unwrap_or(i64::MAX)),
        None => delay,
    }
}

/// Insert the reservation and open its first attempt, inside the caller's
/// transaction. `None` means the occurrence/delivery was already reserved —
/// `UNIQUE (binding_id, dedup_key)` is the whole crash-consistency mechanism, so
/// a conflict is a normal outcome and never an error.
async fn insert_receipt(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    binding: &BindingRecord,
    dedup_key: &str,
    occurrence_at: Option<String>,
    delivery_id: Option<String>,
    content_fingerprint: Option<String>,
    now: DateTime<Utc>,
) -> Result<Option<Reservation>, CodypendentError> {
    let receipt_id = Uuid::now_v7().to_string();
    let idempotency_key = idempotency_key_for(&binding.id, dedup_key);
    let expires_at = (now + TimeDelta::seconds(binding.dedup_window_seconds.max(0))).to_rfc3339();

    let inserted: Option<(String,)> = sqlx::query_as(
        "INSERT INTO automation_receipts \
         (id, binding_id, dedup_key, delivery_id, content_fingerprint, occurrence_at, \
          reserved_at, expires_at, state, workflow_run_id, workflow_idempotency_key, \
          last_error, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'reserved', NULL, ?, NULL, ?) \
         ON CONFLICT (binding_id, dedup_key) DO NOTHING \
         RETURNING id",
    )
    .bind(&receipt_id)
    .bind(&binding.id)
    .bind(dedup_key)
    .bind(delivery_id.as_deref())
    .bind(content_fingerprint.as_deref())
    .bind(occurrence_at.as_deref())
    .bind(now.to_rfc3339())
    .bind(&expires_at)
    .bind(&idempotency_key)
    .bind(now.to_rfc3339())
    .fetch_optional(&mut **tx)
    .await
    .map_err(database_error)?;

    let Some((receipt_id,)) = inserted else {
        return Ok(None);
    };

    sqlx::query(
        "INSERT INTO automation_attempts (id, receipt_id, attempt, started_at) \
         VALUES (?, ?, 1, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(&receipt_id)
    .bind(now.to_rfc3339())
    .execute(&mut **tx)
    .await
    .map_err(database_error)?;

    Ok(Some(Reservation {
        receipt_id,
        dedup_key: dedup_key.to_string(),
        occurrence_at,
        delivery_id,
        idempotency_key,
        attempt: 1,
    }))
}

/// Open a fresh attempt row on an existing receipt (the retry / re-drive path).
async fn open_attempt(
    pool: &SqlitePool,
    receipt_id: &str,
    attempt: i64,
    now: DateTime<Utc>,
) -> Result<(), CodypendentError> {
    sqlx::query(
        "INSERT INTO automation_attempts (id, receipt_id, attempt, started_at) VALUES (?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(receipt_id)
    .bind(attempt)
    .bind(now.to_rfc3339())
    .execute(pool)
    .await
    .map_err(database_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::{
        AutomationBindingDraft, DeduplicationPolicy, InvocationPolicy, TriggerSource,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn binding_fixture(source_type: &str) -> BindingRecord {
        BindingRecord {
            id: "bind-1".into(),
            owner_uid: 1000,
            source_type: source_type.into(),
            cron_expression: Some("0 2 * * *".into()),
            cron_timezone: Some("UTC".into()),
            one_time_at: None,
            next_fire_at: None,
            last_fire_at: None,
            workflow_id: WorkflowId::new(),
            workflow_version: "1".into(),
            repository_id: RepositoryId::new(),
            repository_path: "/tmp/repo".into(),
            filters: TriggerFilters::default(),
            dedup_window_seconds: 86_400,
            concurrency_text: "allow".into(),
            missed_run_text: "skip".into(),
            missed_run_max_occurrences: None,
            retry: TriggerRetryPolicy::default(),
            approval_mode_text: "inherit".into(),
            approval_receipt: None,
            budget: [None; 4],
            identity_fields: Vec::new(),
        }
    }

    fn policies(concurrency: &str, missed: &str, max: Option<i64>) -> BindingPolicies {
        let mut binding = binding_fixture("cron");
        binding.concurrency_text = concurrency.into();
        binding.missed_run_text = missed.into();
        binding.missed_run_max_occurrences = max;
        binding.policies().expect("policies parse")
    }

    // -- fail-closed policy reads ----------------------------------------

    #[test]
    fn an_unknown_concurrency_column_refuses() {
        let mut binding = binding_fixture("cron");
        binding.concurrency_text = "stampede".into();
        let error = binding.policies().expect_err("must refuse");
        assert_eq!(error.code, "automation.unknown-policy");
        assert!(!error.retryable);
    }

    #[test]
    fn an_unknown_missed_run_column_refuses_rather_than_catching_up() {
        let mut binding = binding_fixture("cron");
        binding.missed_run_text = "replay_everything".into();
        let error = binding.policies().expect_err("must refuse");
        assert_eq!(error.code, "automation.unknown-policy");
    }

    #[test]
    fn an_unknown_approval_mode_refuses_rather_than_inheriting() {
        let mut binding = binding_fixture("cron");
        binding.approval_mode_text = "trust_me".into();
        let error = binding.policies().expect_err("must refuse");
        assert_eq!(error.code, "automation.unknown-policy");
    }

    #[test]
    fn catch_up_without_a_bound_refuses() {
        let mut binding = binding_fixture("cron");
        binding.missed_run_text = "catch_up".into();
        binding.missed_run_max_occurrences = None;
        assert_eq!(
            binding.policies().expect_err("must refuse").code,
            "automation.unknown-policy"
        );
    }

    // -- the schedule decision -------------------------------------------

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("fixture instant")
            .with_timezone(&Utc)
    }

    #[test]
    fn a_fresh_due_occurrence_fires_and_rearms() {
        let binding = binding_fixture("cron");
        let policies = policies("allow", "skip", None);
        let due = at("2026-03-01T02:00:00+00:00");
        let decision = decide_schedule(&binding, &policies, due, due, MISSED_RUN_GRACE_SECONDS);
        match decision {
            ScheduleDecision::Fire { occurrence, next } => {
                assert_eq!(occurrence, due);
                assert_eq!(next, Some(at("2026-03-02T02:00:00+00:00")));
            }
            other => panic!("expected a firing, got {other:?}"),
        }
    }

    #[test]
    fn a_missed_backlog_under_skip_rearms_without_firing() {
        let binding = binding_fixture("cron");
        let policies = policies("allow", "skip", None);
        let due = at("2026-03-01T02:00:00+00:00");
        let now = at("2026-03-10T09:00:00+00:00");
        match decide_schedule(&binding, &policies, due, now, MISSED_RUN_GRACE_SECONDS) {
            ScheduleDecision::Advance { next } => {
                assert_eq!(next, Some(at("2026-03-11T02:00:00+00:00")));
            }
            other => panic!("skip must not fire, got {other:?}"),
        }
    }

    #[test]
    fn run_once_fires_the_most_recent_missed_occurrence() {
        let binding = binding_fixture("cron");
        let policies = policies("allow", "run_once", None);
        let due = at("2026-03-01T02:00:00+00:00");
        let now = at("2026-03-10T09:00:00+00:00");
        match decide_schedule(&binding, &policies, due, now, MISSED_RUN_GRACE_SECONDS) {
            ScheduleDecision::Fire { occurrence, next } => {
                assert_eq!(occurrence, at("2026-03-10T02:00:00+00:00"));
                assert_eq!(next, Some(at("2026-03-11T02:00:00+00:00")));
            }
            other => panic!("expected one firing, got {other:?}"),
        }
    }

    #[test]
    fn catch_up_is_bounded_by_max_occurrences() {
        let binding = binding_fixture("cron");
        let policies = policies("allow", "catch_up", Some(2));
        let due = at("2026-03-01T02:00:00+00:00");
        let now = at("2026-03-10T09:00:00+00:00");
        // Nine occurrences are missed; a bound of two must start from the
        // second-newest, never from the oldest.
        match decide_schedule(&binding, &policies, due, now, MISSED_RUN_GRACE_SECONDS) {
            ScheduleDecision::Fire { occurrence, next } => {
                assert_eq!(occurrence, at("2026-03-09T02:00:00+00:00"));
                assert_eq!(next, Some(at("2026-03-10T02:00:00+00:00")));
            }
            other => panic!("expected a bounded catch-up, got {other:?}"),
        }
    }

    #[test]
    fn an_unscheduled_source_carrying_next_fire_at_refuses() {
        let binding = binding_fixture("github_webhook");
        let policies = policies("allow", "skip", None);
        let due = at("2026-03-01T02:00:00+00:00");
        match decide_schedule(&binding, &policies, due, due, MISSED_RUN_GRACE_SECONDS) {
            ScheduleDecision::Refuse(error) => assert_eq!(error.code, "automation.not-scheduled"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_one_time_binding_clears_its_schedule_after_firing() {
        let mut binding = binding_fixture("one_time");
        binding.one_time_at = Some("2026-03-01T02:00:00+00:00".into());
        let policies = policies("allow", "skip", None);
        let due = at("2026-03-01T02:00:00+00:00");
        match decide_schedule(&binding, &policies, due, due, MISSED_RUN_GRACE_SECONDS) {
            ScheduleDecision::Fire { occurrence, next } => {
                assert_eq!(occurrence, due);
                assert_eq!(next, None, "a fired one-time binding is never due again");
            }
            other => panic!("expected a firing, got {other:?}"),
        }
    }

    // -- filters ----------------------------------------------------------

    #[test]
    fn a_filter_dimension_with_no_normalized_field_does_not_match() {
        let filters = TriggerFilters {
            paths: vec!["src/**".into()],
            ..Default::default()
        };
        let event = TriggerEvent {
            event_type: "push".into(),
            ..Default::default()
        };
        assert!(
            !event_matches_filters(&filters, &event),
            "a path filter we cannot evaluate must narrow, never widen"
        );
    }

    #[test]
    fn a_branch_filter_matches_a_fully_qualified_ref() {
        let filters = TriggerFilters {
            branches: vec!["main".into()],
            ..Default::default()
        };
        let mut event = TriggerEvent {
            event_type: "push".into(),
            ..Default::default()
        };
        event
            .fields
            .insert("git_ref".into(), "refs/heads/main".into());
        assert!(event_matches_filters(&filters, &event));
        event
            .fields
            .insert("git_ref".into(), "refs/heads/release".into());
        assert!(!event_matches_filters(&filters, &event));
    }

    #[test]
    fn dedup_refuses_when_a_named_identity_field_is_absent() {
        let mut binding = binding_fixture("github_webhook");
        binding.identity_fields = vec!["number".into()];
        let event = TriggerEvent {
            event_type: "pull_request".into(),
            delivery_id: Some("d-1".into()),
            ..Default::default()
        };
        assert_eq!(
            derive_dedup_key(&binding, &event)
                .expect_err("must refuse")
                .code,
            "automation.dedup-field-missing"
        );
    }

    // -- retry backoff -----------------------------------------------------

    #[test]
    fn retry_backoff_is_exponential_and_capped() {
        let retry = TriggerRetryPolicy {
            max_attempts: 5,
            initial_delay_seconds: 10,
            backoff_multiplier: 3,
            max_delay_seconds: Some(100),
        };
        assert_eq!(retry_delay_seconds(&retry, 1), 10);
        assert_eq!(retry_delay_seconds(&retry, 2), 30);
        assert_eq!(retry_delay_seconds(&retry, 3), 90);
        assert_eq!(
            retry_delay_seconds(&retry, 4),
            100,
            "capped, never unbounded"
        );
    }

    // -- the database-backed guarantees ------------------------------------

    struct CountingStarter {
        starts: AtomicUsize,
        fail_with: Option<CodypendentError>,
    }

    impl WorkflowStarter for CountingStarter {
        fn start(
            &self,
            request: StartWorkflowRequest,
        ) -> crate::workflows::WorkflowStartFuture<'_> {
            let key = request.idempotency_key.clone();
            Box::pin(async move {
                if let Some(error) = &self.fail_with {
                    return Err(error.clone());
                }
                self.starts.fetch_add(1, Ordering::SeqCst);
                // Deterministic in the key, as the real store's
                // `deterministic_run_id("owner:{uid}:{key}")` is.
                Ok(format!(
                    "wfrun-{}",
                    hex::encode(sha2::Sha256::digest(key.as_bytes()))
                ))
            })
        }
    }

    /// A starter that records the ceiling each dispatch actually handed across the
    /// seam, so a test can prove the binding's ceiling TRAVELS rather than merely
    /// that the firing was not denied.
    struct RecordingStarter {
        ceilings: std::sync::Mutex<Vec<Option<WorkflowRunCeiling>>>,
    }

    impl RecordingStarter {
        fn new() -> Self {
            Self {
                ceilings: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl WorkflowStarter for RecordingStarter {
        fn start(
            &self,
            request: StartWorkflowRequest,
        ) -> crate::workflows::WorkflowStartFuture<'_> {
            let key = request.idempotency_key.clone();
            self.ceilings
                .lock()
                .expect("ceilings lock")
                .push(request.budget_ceiling);
            Box::pin(async move {
                Ok(format!(
                    "wfrun-{}",
                    hex::encode(sha2::Sha256::digest(key.as_bytes()))
                ))
            })
        }
    }

    struct StubEnvironment {
        repository_id: RepositoryId,
        owner_ok: bool,
        workflow: Result<String, CodypendentError>,
    }

    impl AutomationEnvironment for StubEnvironment {
        fn resolve_workflow(
            &self,
            _workflow_id: WorkflowId,
            _workflow_version: &str,
            _repository_path: &Path,
        ) -> Result<String, CodypendentError> {
            self.workflow.clone()
        }
        fn repository_id_for(&self, _path: &Path) -> Option<RepositoryId> {
            Some(self.repository_id)
        }
        fn owner_is_resolvable(&self, _uid: u32) -> bool {
            self.owner_ok
        }
    }

    use sha2::Digest as _;

    async fn pool() -> SqlitePool {
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
        crate::db::open_database(&dir.path().join("automation.db"))
            .await
            .expect("open database")
    }

    /// Insert a binding through the PRODUCTION create path, so the test cannot
    /// drift from what `create_binding` actually writes.
    async fn seed_binding(
        pool: &SqlitePool,
        uid: u32,
        repository_id: RepositoryId,
        source: TriggerSource,
        invocation: InvocationPolicy,
    ) -> String {
        // `create_binding` resolves the repository path out of `sessions`, so a
        // session for this owner must exist first — the same precondition a real
        // client has.
        sqlx::query(
            "INSERT INTO sessions (id, title, state, created_at, updated_at, repository, \
             repository_id, owner_uid) VALUES (?, 'seed', 'active', ?, ?, '/tmp/repo', ?, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(Utc::now().to_rfc3339())
        .bind(Utc::now().to_rfc3339())
        .bind(repository_id.to_string())
        .bind(i64::from(uid))
        .execute(pool)
        .await
        .expect("seed session");

        let binding = crate::automation::create_binding(
            pool,
            crate::principal::PeerPrincipal::from_uid(uid),
            AutomationBindingDraft {
                name: format!("nightly-{}", Uuid::now_v7()),
                source,
                workflow_id: WorkflowId::new(),
                workflow_version: "1".into(),
                repository_id,
                filters: TriggerFilters::default(),
                invocation,
                enabled: true,
            },
        )
        .await
        .expect("create binding");
        binding.id.to_string()
    }

    fn scheduler(
        pool: &SqlitePool,
        starter: Arc<CountingStarter>,
        environment: Arc<StubEnvironment>,
    ) -> AutomationScheduler {
        AutomationScheduler::new(pool.clone(), DaemonInstanceId::new(), starter, environment)
    }

    /// The same wiring for any starter double (the recording one included).
    fn scheduler_with<S: WorkflowStarter + 'static>(
        pool: &SqlitePool,
        starter: Arc<S>,
        environment: Arc<StubEnvironment>,
    ) -> AutomationScheduler {
        AutomationScheduler::new(pool.clone(), DaemonInstanceId::new(), starter, environment)
    }

    /// THE guarantee: a due binding fires exactly once even when several ticks
    /// race it. Two schedulers with DIFFERENT instance ids contend for the same
    /// binding concurrently — the lease, the compare-and-swap and the receipt's
    /// UNIQUE constraint must between them admit exactly one firing.
    #[tokio::test]
    async fn a_due_binding_fires_exactly_once_under_concurrent_ticks() {
        let pool = pool().await;
        let repository_id = RepositoryId::new();
        let binding_id = seed_binding(
            &pool,
            1000,
            repository_id,
            TriggerSource::OneTime {
                at: Utc::now() - TimeDelta::seconds(5),
            },
            InvocationPolicy::default(),
        )
        .await;

        let starter = Arc::new(CountingStarter {
            starts: AtomicUsize::new(0),
            fail_with: None,
        });
        let environment = Arc::new(StubEnvironment {
            repository_id,
            owner_ok: true,
            workflow: Ok("repair-github-check".into()),
        });

        let now = Utc::now();
        let a = scheduler(&pool, starter.clone(), environment.clone());
        let b = scheduler(&pool, starter.clone(), environment.clone());
        let c = scheduler(&pool, starter.clone(), environment.clone());
        let (ra, rb, rc) = tokio::join!(a.tick(now), b.tick(now), c.tick(now));

        assert_eq!(
            starter.starts.load(Ordering::SeqCst),
            1,
            "three racing ticks must start exactly one run (a={ra:?} b={rb:?} c={rc:?})"
        );

        let receipts: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM automation_receipts WHERE binding_id = ?")
                .bind(&binding_id)
                .fetch_one(&pool)
                .await
                .expect("count receipts");
        assert_eq!(receipts, 1, "exactly one reservation exists");

        let (state, run_id): (String, Option<String>) = sqlx::query_as(
            "SELECT state, workflow_run_id FROM automation_receipts WHERE binding_id = ?",
        )
        .bind(&binding_id)
        .fetch_one(&pool)
        .await
        .expect("read receipt");
        assert_eq!(state, "dispatched");
        assert!(run_id.is_some(), "the run id is recorded on the receipt");

        // The one-time schedule is cleared, so a later tick finds nothing due —
        // the clearing path `validate_and_project_source` never had.
        let next: Option<String> =
            sqlx::query_scalar("SELECT next_fire_at FROM automation_bindings WHERE id = ?")
                .bind(&binding_id)
                .fetch_one(&pool)
                .await
                .expect("read next_fire_at");
        assert_eq!(next, None);

        let again = a.tick(Utc::now()).await;
        assert_eq!(
            again.claimed, 0,
            "a fired one-time binding is not re-claimed"
        );
        assert_eq!(starter.starts.load(Ordering::SeqCst), 1);
    }

    /// A disabled binding never fires, even with a `next_fire_at` in the past.
    #[tokio::test]
    async fn a_disabled_binding_never_fires() {
        let pool = pool().await;
        let repository_id = RepositoryId::new();
        let binding_id = seed_binding(
            &pool,
            1000,
            repository_id,
            TriggerSource::OneTime {
                at: Utc::now() - TimeDelta::seconds(5),
            },
            InvocationPolicy::default(),
        )
        .await;
        sqlx::query("UPDATE automation_bindings SET enabled = 0 WHERE id = ?")
            .bind(&binding_id)
            .execute(&pool)
            .await
            .expect("disable");

        let starter = Arc::new(CountingStarter {
            starts: AtomicUsize::new(0),
            fail_with: None,
        });
        let scheduler = scheduler(
            &pool,
            starter.clone(),
            Arc::new(StubEnvironment {
                repository_id,
                owner_ok: true,
                workflow: Ok("repair-github-check".into()),
            }),
        );
        let report = scheduler.tick(Utc::now()).await;
        assert_eq!(report.examined, 0, "a disabled binding is not even due");
        assert_eq!(starter.starts.load(Ordering::SeqCst), 0);
        let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM automation_receipts")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(receipts, 0, "nothing was reserved for a disabled binding");
    }

    /// An owner that no longer resolves is denied — never silently downgraded to
    /// the daemon's own uid — and the denial is recorded honestly.
    #[tokio::test]
    async fn an_unresolvable_owner_is_denied_and_never_started() {
        let pool = pool().await;
        let repository_id = RepositoryId::new();
        let binding_id = seed_binding(
            &pool,
            4242,
            repository_id,
            TriggerSource::OneTime {
                at: Utc::now() - TimeDelta::seconds(5),
            },
            InvocationPolicy::default(),
        )
        .await;

        let starter = Arc::new(CountingStarter {
            starts: AtomicUsize::new(0),
            fail_with: None,
        });
        let scheduler = scheduler(
            &pool,
            starter.clone(),
            Arc::new(StubEnvironment {
                repository_id,
                owner_ok: false,
                workflow: Ok("repair-github-check".into()),
            }),
        );
        scheduler.tick(Utc::now()).await;

        assert_eq!(starter.starts.load(Ordering::SeqCst), 0);
        let (state, error, outcome): (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT r.state, r.last_error, a.outcome FROM automation_receipts r \
             JOIN automation_attempts a ON a.receipt_id = r.id WHERE r.binding_id = ?",
        )
        .bind(&binding_id)
        .fetch_one(&pool)
        .await
        .expect("read receipt + attempt");
        assert_eq!(state, "failed");
        assert_eq!(error.as_deref(), Some("automation.owner-unresolvable"));
        assert_eq!(outcome.as_deref(), Some("denied"));
    }

    /// A binding whose declared budget ceiling cannot be carried into the run is
    /// refused rather than run without it.
    #[tokio::test]
    async fn a_budget_ceiling_that_cannot_be_enforced_refuses() {
        let pool = pool().await;
        let repository_id = RepositoryId::new();
        let invocation = InvocationPolicy {
            budget_ceiling: Some(codypendent_protocol::BudgetCeiling {
                tokens: Some(1000),
                ..Default::default()
            }),
            ..Default::default()
        };
        let binding_id = seed_binding(
            &pool,
            1000,
            repository_id,
            TriggerSource::OneTime {
                at: Utc::now() - TimeDelta::seconds(5),
            },
            invocation,
        )
        .await;

        let starter = Arc::new(CountingStarter {
            starts: AtomicUsize::new(0),
            fail_with: None,
        });
        let scheduler = scheduler(
            &pool,
            starter.clone(),
            Arc::new(StubEnvironment {
                repository_id,
                owner_ok: true,
                workflow: Ok("repair-github-check".into()),
            }),
        );
        scheduler.tick(Utc::now()).await;

        assert_eq!(
            starter.starts.load(Ordering::SeqCst),
            0,
            "a ceiling that cannot be enforced must not be silently dropped"
        );
        let error: Option<String> =
            sqlx::query_scalar("SELECT last_error FROM automation_receipts WHERE binding_id = ?")
                .bind(&binding_id)
                .fetch_one(&pool)
                .await
                .expect("read receipt");
        assert_eq!(error.as_deref(), Some("automation.budget-unenforceable"));
    }

    /// The relaxation: a ceiling made ONLY of dimensions the run's budget
    /// envelope charges (wall-time, cost) now fires, and the ceiling reaches the
    /// start seam. Reverting the carry makes this fail at the `starts == 1`
    /// assertion (the gate denies again) as well as at the recorded ceiling.
    #[tokio::test]
    async fn an_enforceable_budget_ceiling_is_carried_into_the_run() {
        let pool = pool().await;
        let repository_id = RepositoryId::new();
        let invocation = InvocationPolicy {
            budget_ceiling: Some(codypendent_protocol::BudgetCeiling {
                wall_time_seconds: Some(1_800),
                cost_micros: Some(2_500_000),
                ..Default::default()
            }),
            ..Default::default()
        };
        let binding_id = seed_binding(
            &pool,
            1000,
            repository_id,
            TriggerSource::OneTime {
                at: Utc::now() - TimeDelta::seconds(5),
            },
            invocation,
        )
        .await;

        let starter = Arc::new(RecordingStarter::new());
        let scheduler = scheduler_with(
            &pool,
            starter.clone(),
            Arc::new(StubEnvironment {
                repository_id,
                owner_ok: true,
                workflow: Ok("repair-github-check".into()),
            }),
        );
        scheduler.tick(Utc::now()).await;

        let ceilings = starter.ceilings.lock().expect("ceilings lock").clone();
        assert_eq!(ceilings.len(), 1, "the firing dispatched exactly once");
        assert_eq!(
            ceilings[0],
            Some(WorkflowRunCeiling {
                max_wall_time_seconds: Some(1_800),
                max_cost_micros: Some(2_500_000),
            }),
            "the binding row's ceiling must cross the start seam, not be dropped"
        );

        let (state, error): (String, Option<String>) = sqlx::query_as(
            "SELECT state, last_error FROM automation_receipts WHERE binding_id = ?",
        )
        .bind(&binding_id)
        .fetch_one(&pool)
        .await
        .expect("read receipt");
        assert_eq!(state, "dispatched");
        assert_eq!(error, None);
    }

    /// A tool-call cap keeps refusing: the workflow envelope has no tool-call
    /// dimension, so carrying it would drop it.
    #[tokio::test]
    async fn a_tool_call_ceiling_still_refuses() {
        let pool = pool().await;
        let repository_id = RepositoryId::new();
        let invocation = InvocationPolicy {
            budget_ceiling: Some(codypendent_protocol::BudgetCeiling {
                tool_calls: Some(25),
                ..Default::default()
            }),
            ..Default::default()
        };
        let binding_id = seed_binding(
            &pool,
            1000,
            repository_id,
            TriggerSource::OneTime {
                at: Utc::now() - TimeDelta::seconds(5),
            },
            invocation,
        )
        .await;

        let starter = Arc::new(RecordingStarter::new());
        let scheduler = scheduler_with(
            &pool,
            starter.clone(),
            Arc::new(StubEnvironment {
                repository_id,
                owner_ok: true,
                workflow: Ok("repair-github-check".into()),
            }),
        );
        scheduler.tick(Utc::now()).await;

        assert!(
            starter.ceilings.lock().expect("ceilings lock").is_empty(),
            "an unenforceable dimension must not reach the start seam at all"
        );
        let error: Option<String> =
            sqlx::query_scalar("SELECT last_error FROM automation_receipts WHERE binding_id = ?")
                .bind(&binding_id)
                .fetch_one(&pool)
                .await
                .expect("read receipt");
        assert_eq!(error.as_deref(), Some("automation.budget-unenforceable"));
    }

    /// Every non-`inherit` approval mode is still refused, and the refusal is
    /// recorded — nothing downstream can honour one, so a firing that looked
    /// approval-gated would run its effects unapproved.
    #[tokio::test]
    async fn a_non_inherit_approval_mode_still_refuses() {
        let pool = pool().await;
        let repository_id = RepositoryId::new();
        let invocation = InvocationPolicy {
            approval_mode: AutomationApprovalMode::AlwaysRequire,
            ..Default::default()
        };
        let binding_id = seed_binding(
            &pool,
            1000,
            repository_id,
            TriggerSource::OneTime {
                at: Utc::now() - TimeDelta::seconds(5),
            },
            invocation,
        )
        .await;

        let starter = Arc::new(RecordingStarter::new());
        let scheduler = scheduler_with(
            &pool,
            starter.clone(),
            Arc::new(StubEnvironment {
                repository_id,
                owner_ok: true,
                workflow: Ok("repair-github-check".into()),
            }),
        );
        scheduler.tick(Utc::now()).await;

        assert!(starter.ceilings.lock().expect("ceilings lock").is_empty());
        let (error, outcome): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT r.last_error, a.outcome FROM automation_receipts r \
             JOIN automation_attempts a ON a.receipt_id = r.id WHERE r.binding_id = ?",
        )
        .bind(&binding_id)
        .fetch_one(&pool)
        .await
        .expect("read receipt + attempt");
        assert_eq!(error.as_deref(), Some("automation.approval-unenforceable"));
        assert_eq!(outcome.as_deref(), Some("denied"));
    }

    /// A binding whose workflow no longer resolves is denied, and nothing is
    /// started against a different workflow.
    #[tokio::test]
    async fn an_unresolvable_workflow_is_denied() {
        let pool = pool().await;
        let repository_id = RepositoryId::new();
        let binding_id = seed_binding(
            &pool,
            1000,
            repository_id,
            TriggerSource::OneTime {
                at: Utc::now() - TimeDelta::seconds(5),
            },
            InvocationPolicy::default(),
        )
        .await;

        let starter = Arc::new(CountingStarter {
            starts: AtomicUsize::new(0),
            fail_with: None,
        });
        let scheduler = scheduler(
            &pool,
            starter.clone(),
            Arc::new(StubEnvironment {
                repository_id,
                owner_ok: true,
                workflow: Err(refuse(
                    "automation.workflow-unresolvable",
                    "no manifest hashes to this workflow id",
                )),
            }),
        );
        scheduler.tick(Utc::now()).await;
        assert_eq!(starter.starts.load(Ordering::SeqCst), 0);
        let error: Option<String> =
            sqlx::query_scalar("SELECT last_error FROM automation_receipts WHERE binding_id = ?")
                .bind(&binding_id)
                .fetch_one(&pool)
                .await
                .expect("read receipt");
        assert_eq!(error.as_deref(), Some("automation.workflow-unresolvable"));
    }

    /// A webhook delivery reaches exactly one run per binding, and a redelivery
    /// under the SAME identity reserves nothing further.
    #[tokio::test]
    async fn a_webhook_delivery_starts_a_run_once_per_binding() {
        let pool = pool().await;
        let repository_id = RepositoryId::new();
        let binding_id = seed_binding(
            &pool,
            1000,
            repository_id,
            TriggerSource::GitHubWebhook {
                endpoint_id: "ep-alpha".into(),
                installation_id: None,
                events: vec!["pull_request".into()],
            },
            InvocationPolicy {
                deduplication: DeduplicationPolicy {
                    identity_fields: vec!["repository".into(), "number".into()],
                    window_seconds: 600,
                },
                ..Default::default()
            },
        )
        .await;

        let starter = Arc::new(CountingStarter {
            starts: AtomicUsize::new(0),
            fail_with: None,
        });
        let scheduler = scheduler(
            &pool,
            starter.clone(),
            Arc::new(StubEnvironment {
                repository_id,
                owner_ok: true,
                workflow: Ok("repair-github-check".into()),
            }),
        );

        let mut event = TriggerEvent {
            event_type: "pull_request".into(),
            delivery_id: Some("delivery-1".into()),
            content_fingerprint: Some("body-sha256:abc".into()),
            fields: BTreeMap::new(),
        };
        event
            .fields
            .insert("repository".into(), "octo/hello".into());
        event.fields.insert("number".into(), "7".into());
        event.fields.insert("action".into(), "opened".into());

        let now = Utc::now();
        let first = scheduler
            .dispatch_endpoint_event("ep-alpha", &event, now)
            .await;
        assert_eq!(first, 1, "the delivery reached the binding");
        assert_eq!(starter.starts.load(Ordering::SeqCst), 1);

        // A redelivery with a NEW provider delivery id but the same server-derived
        // identity must not start a second run.
        let mut redelivery = event.clone();
        redelivery.delivery_id = Some("delivery-2".into());
        let second = scheduler
            .dispatch_endpoint_event("ep-alpha", &redelivery, now)
            .await;
        assert_eq!(second, 0, "the identity was already reserved");
        assert_eq!(starter.starts.load(Ordering::SeqCst), 1);

        let receipts: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM automation_receipts WHERE binding_id = ?")
                .bind(&binding_id)
                .fetch_one(&pool)
                .await
                .expect("count receipts");
        assert_eq!(receipts, 1);
    }

    /// A delivery a binding's filters exclude is recorded as `filtered` and
    /// starts nothing.
    #[tokio::test]
    async fn a_filtered_delivery_records_the_filtering_and_starts_nothing() {
        let pool = pool().await;
        let repository_id = RepositoryId::new();
        let binding_id = seed_binding(
            &pool,
            1000,
            repository_id,
            TriggerSource::GitHubWebhook {
                endpoint_id: "ep-beta".into(),
                installation_id: None,
                events: Vec::new(),
            },
            InvocationPolicy::default(),
        )
        .await;
        // Narrow it to `main` after creation, through the same column the
        // production create path writes.
        sqlx::query("UPDATE automation_bindings SET filters_json = ? WHERE id = ?")
            .bind(r#"{"branches":["main"]}"#)
            .bind(&binding_id)
            .execute(&pool)
            .await
            .expect("narrow filters");

        let starter = Arc::new(CountingStarter {
            starts: AtomicUsize::new(0),
            fail_with: None,
        });
        let scheduler = scheduler(
            &pool,
            starter.clone(),
            Arc::new(StubEnvironment {
                repository_id,
                owner_ok: true,
                workflow: Ok("repair-github-check".into()),
            }),
        );

        let mut event = TriggerEvent {
            event_type: "push".into(),
            delivery_id: Some("delivery-9".into()),
            content_fingerprint: None,
            fields: BTreeMap::new(),
        };
        event
            .fields
            .insert("git_ref".into(), "refs/heads/experiment".into());

        scheduler
            .dispatch_endpoint_event("ep-beta", &event, Utc::now())
            .await;
        assert_eq!(starter.starts.load(Ordering::SeqCst), 0);
        let outcome: Option<String> = sqlx::query_scalar(
            "SELECT a.outcome FROM automation_attempts a \
             JOIN automation_receipts r ON r.id = a.receipt_id WHERE r.binding_id = ?",
        )
        .bind(&binding_id)
        .fetch_one(&pool)
        .await
        .expect("read attempt");
        assert_eq!(outcome.as_deref(), Some("filtered"));
    }

    /// A retryable start failure schedules a retry rather than losing the
    /// occurrence, and the retry sweep re-drives the SAME receipt.
    #[tokio::test]
    async fn a_retryable_start_failure_is_retried_not_dropped() {
        let pool = pool().await;
        let repository_id = RepositoryId::new();
        let binding_id = seed_binding(
            &pool,
            1000,
            repository_id,
            TriggerSource::OneTime {
                at: Utc::now() - TimeDelta::seconds(5),
            },
            InvocationPolicy {
                retry: TriggerRetryPolicy {
                    max_attempts: 3,
                    initial_delay_seconds: 1,
                    backoff_multiplier: 1,
                    max_delay_seconds: None,
                },
                ..Default::default()
            },
        )
        .await;

        let failing = Arc::new(CountingStarter {
            starts: AtomicUsize::new(0),
            fail_with: Some(CodypendentError::new(
                "workflow.store-error",
                "database is busy",
                true,
            )),
        });
        let environment = Arc::new(StubEnvironment {
            repository_id,
            owner_ok: true,
            workflow: Ok("repair-github-check".into()),
        });
        let scheduler = scheduler(&pool, failing, environment.clone());
        scheduler.tick(Utc::now()).await;

        let (state, attempt_open): (String, i64) = sqlx::query_as(
            "SELECT r.state, COUNT(a.id) FROM automation_receipts r \
             JOIN automation_attempts a ON a.receipt_id = r.id \
             WHERE r.binding_id = ? AND a.finished_at IS NULL AND a.next_retry_at IS NOT NULL \
             GROUP BY r.id",
        )
        .bind(&binding_id)
        .fetch_one(&pool)
        .await
        .expect("read receipt");
        assert_eq!(state, "reserved", "the occurrence is still owed a run");
        assert_eq!(attempt_open, 1, "a retry is armed");

        // The sweep, once the retry is due, re-drives the same receipt against a
        // healthy starter.
        let healthy = Arc::new(CountingStarter {
            starts: AtomicUsize::new(0),
            fail_with: None,
        });
        let recovered = AutomationScheduler::new(
            pool.clone(),
            DaemonInstanceId::new(),
            healthy.clone(),
            environment,
        );
        recovered.tick(Utc::now() + TimeDelta::seconds(30)).await;
        assert_eq!(healthy.starts.load(Ordering::SeqCst), 1);
        let state: String =
            sqlx::query_scalar("SELECT state FROM automation_receipts WHERE binding_id = ?")
                .bind(&binding_id)
                .fetch_one(&pool)
                .await
                .expect("read receipt");
        assert_eq!(state, "dispatched");
    }
}
