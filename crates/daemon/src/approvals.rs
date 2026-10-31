//! Approval broker (STEP 1.6).
//!
//! An approval is a **workflow state**, not a UI modal
//! ([Chapter 04](../../../docs/docs/04-agent-runtime-and-workflows.md)): a run
//! that proposes a side effect requiring human sign-off *parks* in
//! `WaitingForApproval` until an approver resolves it. This module owns the
//! parking mechanism and the durable record; the run-state transition itself is
//! the agent loop's concern (STEP 1.10).
//!
//! ## How a caller awaits a decision
//!
//! [`ApprovalBroker::request`] persists a `pending` row, appends an
//! `ApprovalRequested` event, registers an in-memory waiter, publishes that
//! event to any live subscribers (when the broker is bound to a
//! [`SubscriptionHub`] via [`ApprovalBroker::with_subscriptions`]), and returns
//! the new [`ApprovalId`]. The awaiting run then calls
//! [`ApprovalBroker::await_decision`], which blocks until the waiter is woken by
//! [`ApprovalBroker::resolve`] (a human decision) or
//! [`ApprovalBroker::expire_due`] (a timeout, which behaves as a rejection).
//! Splitting `request` from `await_decision` (rather than returning a
//! `oneshot::Receiver`) lets restart recovery re-register waiters — see
//! [`ApprovalBroker::reload_pending`] — so a resuming run simply calls
//! `await_decision` again; nothing is lost.
//!
//! ## Auto-approval
//!
//! Resolving with [`ApprovalScope::Run`] records the approved row as a *pattern*
//! for its run. [`ApprovalBroker::request`] consults these first: if an identical
//! action (same `run_id` and the same [`action_digest`]) was already approved
//! `Run`-scoped, the new request auto-approves immediately — it still writes an
//! `approved` row and `ApprovalRequested`/`ApprovalResolved` events for
//! auditability — instead of parking. The matching key is `run_id` + the hex
//! SHA-256 of the action's canonical JSON serialization.
//!
//! Waiters live behind a [`std::sync::Mutex`]-guarded map keyed by
//! [`ApprovalId`], each a [`tokio::sync::watch`] channel carrying the eventual
//! [`ApprovalDecision`]. The map is only ever locked for synchronous map
//! operations (never across an `.await`), so a std mutex is the right primitive.
//! `watch` retains the last value, so a decision delivered before the run
//! subscribes is never lost, and multiple observers (a resuming run, an attached
//! client) can subscribe independently. The map is locked through
//! [`lock_recovering`](crate::poison::lock_recovering): a panic elsewhere must
//! not leave every later approval unanswerable, and no partial update of this
//! map can fabricate a decision (see that module's docs).

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use codypendent_protocol::{
    Actor, ApprovalDecision, ApprovalId, ApprovalScope, EventBody, ProposedAction, Risk, RunId,
    SessionEvent, SessionId, UserId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::sync::watch;

use crate::poison::lock_recovering;
use crate::policy::Capability;
use crate::subscriptions::SubscriptionHub;

/// The lifecycle state of an approval row, mirroring the `state` column
/// (`pending | approved | rejected | expired`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalState {
    /// Awaiting a human decision.
    Pending,
    /// Resolved as approved.
    Approved,
    /// Resolved as rejected.
    Rejected,
    /// Timed out past `expires_at`; behaves as a rejection.
    Expired,
}

impl ApprovalState {
    fn as_db(self) -> &'static str {
        match self {
            ApprovalState::Pending => "pending",
            ApprovalState::Approved => "approved",
            ApprovalState::Rejected => "rejected",
            ApprovalState::Expired => "expired",
        }
    }
}

/// A `pending` approval re-surfaced on daemon restart by
/// [`ApprovalBroker::reload_pending`]. Carries everything a newly attached
/// client needs to re-render the request and everything a resuming run needs to
/// re-await it.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub approval_id: ApprovalId,
    pub run_id: RunId,
    pub session_id: SessionId,
    pub action: ProposedAction,
    pub risk: Risk,
    pub capabilities: Vec<Capability>,
    pub requested_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Which reuse rule auto-approved a request — recorded verbatim in
/// `approvals.resolved_by` so the audit trail names the authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoApproval {
    /// Byte-identical action already Run-approved (existing behavior).
    RunDigest,
    /// A Pattern-scoped approval in this run covers the prefix.
    RunPattern { pattern: String },
    /// A persisted repository rule covers the prefix.
    RepositoryRule { rule_id: String, pattern: String },
}

impl AutoApproval {
    #[must_use]
    pub fn resolved_by(&self) -> String {
        match self {
            AutoApproval::RunDigest => "auto:run-scope".to_string(),
            AutoApproval::RunPattern { pattern } => format!("auto:pattern:{pattern}"),
            AutoApproval::RepositoryRule { rule_id, .. } => format!("auto:repo-rule:{rule_id}"),
        }
    }
}

/// A structured approval-broker error. Every variant is machine-branchable; raw
/// `sqlx`/`serde` failures are wrapped, never surfaced verbatim.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalError {
    /// No approval row exists for the given id.
    #[error("no approval with id {approval_id}")]
    NotFound { approval_id: ApprovalId },
    /// The approval is no longer `pending` (already approved, rejected, or
    /// expired). Distinct from a lost race so callers can branch on it.
    #[error("approval {approval_id} is already resolved (state {state})")]
    AlreadyResolved {
        approval_id: ApprovalId,
        state: String,
    },
    /// `resolve` was handed a decision other than `Approve`/`Reject`.
    #[error("unsupported approval decision (expected Approve or Reject)")]
    UnsupportedDecision,
    /// `resolve` was handed a scope this build does not recognize.
    #[error("unsupported approval scope")]
    UnsupportedScope,
    /// The in-memory waiter was dropped before any decision was recorded (the
    /// broker was torn down while a run was still parked).
    #[error("approval {approval_id} waiter dropped before a decision")]
    WaiterGone { approval_id: ApprovalId },
    #[error("session is closed")]
    SessionClosed,
    /// Pattern or repository scope requested for an unlearnable action or missing repository.
    #[error(
        "this action cannot be generalized to a rule — approve it Once or for the Run instead"
    )]
    PatternUnavailable,
    /// A stored row could not be decoded (should never happen; the daemon wrote
    /// it).
    #[error("corrupt approval row: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

async fn enqueue_control_plane_decision(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    approval_id: ApprovalId,
) -> Result<(), ApprovalError> {
    crate::control_plane_sync::outbox::enqueue_approval_decision_snapshot(
        tx,
        &approval_id.to_string(),
    )
    .await
    .map_err(|error| ApprovalError::Corrupt(format!("control-plane outbox: {error}")))?;
    Ok(())
}

/// The registry of live waiters, shared by every clone of a broker.
type Waiters = Arc<Mutex<HashMap<ApprovalId, watch::Sender<Option<ApprovalDecision>>>>>;

/// Brokers approvals over the `approvals` table plus an in-memory waiter
/// registry.
///
/// Cloning shares one registry (an [`Arc`]), so a run can spawn an awaiter on a
/// clone while another clone resolves — the wake-up still lands. The
/// [`SqlitePool`] is passed per call rather than held, matching the sibling
/// managers in this crate.
#[derive(Debug, Clone, Default)]
pub struct ApprovalBroker {
    waiters: Waiters,
    /// The live fan-out to publish an approval's lifecycle events on, when the
    /// broker is wired into a running daemon. `None` in the executor-less server
    /// and in unit tests, where nothing is attached to observe them (the events
    /// are still persisted — publishing to nobody is what we skip, not the
    /// durable record).
    subscriptions: Option<SubscriptionHub>,
}

impl ApprovalBroker {
    /// A broker with an empty waiter registry and no live fan-out.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind this broker to the shared [`SubscriptionHub`] so [`Self::request`]
    /// publishes its `ApprovalRequested` (and, on auto-approval, `ApprovalResolved`)
    /// to attached clients.
    ///
    /// The agent loop reaches this broker through a pool-erased journal closure
    /// that cannot itself publish (it only sees the pool), so unlike the
    /// human-resolve path — where the `CommandProcessor` re-publishes the
    /// broker's `ApprovalResolved` — the *request* path has no owner of the hub
    /// downstream. Binding the hub here is what lets a live controller see a
    /// parked approval (the TUI builds its pending-approval queue and its
    /// `ResolveApproval` intent from `ApprovalRequested`); without it the run
    /// sits in `WaitingForApproval` until the client re-attaches for catch-up.
    #[must_use]
    pub fn with_subscriptions(mut self, subscriptions: SubscriptionHub) -> Self {
        self.subscriptions = Some(subscriptions);
        self
    }

    /// Persist a `pending` approval, append `ApprovalRequested`, register a
    /// waiter, and return its id — unless an identical action was already
    /// approved `Run`-scoped in this run, in which case auto-approve immediately
    /// (still writing an `approved` row and both events for auditability) and
    /// return without parking.
    ///
    /// The sequence allocation and the ledger append happen inside one
    /// transaction with the row insert, so the append is atomic with respect to
    /// the sequence it claims.
    #[allow(clippy::too_many_arguments)] // signature is normative (STEP 1.6).
    pub async fn request(
        &self,
        pool: &SqlitePool,
        session_id: SessionId,
        run_id: RunId,
        repository: Option<&str>,
        action: ProposedAction,
        risk: Risk,
        capabilities: Vec<Capability>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<ApprovalId, ApprovalError> {
        self.request_with_reuse(
            pool,
            session_id,
            run_id,
            repository,
            action,
            risk,
            capabilities,
            expires_at,
            true,
        )
        .await
    }

    /// Request an approval while explicitly controlling whether a prior
    /// run-scoped approval may be reused. `AlwaysApproval` policy decisions pass
    /// `false`; other approval dispositions retain the normal run-scope behavior.
    #[allow(clippy::too_many_arguments)]
    pub async fn request_with_reuse(
        &self,
        pool: &SqlitePool,
        session_id: SessionId,
        run_id: RunId,
        repository: Option<&str>,
        action: ProposedAction,
        risk: Risk,
        capabilities: Vec<Capability>,
        expires_at: Option<DateTime<Utc>>,
        allow_run_reuse: bool,
    ) -> Result<ApprovalId, ApprovalError> {
        let approval_id = ApprovalId::new();
        self.request_with_id_and_reuse(
            pool,
            approval_id,
            session_id,
            run_id,
            repository,
            action,
            risk,
            capabilities,
            expires_at,
            allow_run_reuse,
        )
        .await
    }

    /// Request an approval using a caller-allocated id. This is used by
    /// durable continuations which must persist their own work record before
    /// making the approval visible; ordinary callers should use
    /// [`Self::request`] or [`Self::request_with_reuse`].
    #[allow(clippy::too_many_arguments)]
    pub async fn request_with_id_and_reuse(
        &self,
        pool: &SqlitePool,
        approval_id: ApprovalId,
        session_id: SessionId,
        run_id: RunId,
        repository: Option<&str>,
        action: ProposedAction,
        risk: Risk,
        capabilities: Vec<Capability>,
        expires_at: Option<DateTime<Utc>>,
        allow_run_reuse: bool,
    ) -> Result<ApprovalId, ApprovalError> {
        let digest = action_digest(&action)?;
        let action_json = serde_json::to_string(&action)?;
        let risk_json = serde_json::to_string(&risk)?;
        let capabilities_json = serde_json::to_string(&capabilities)?;
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let expires_str = expires_at.map(|t| t.to_rfc3339());

        let auto_approve: Option<AutoApproval> = if allow_run_reuse {
            if self.run_scoped_match(pool, run_id, &digest).await? {
                Some(AutoApproval::RunDigest)
            } else {
                self.pattern_match(pool, run_id, repository, &action)
                    .await?
            }
        } else {
            None
        };

        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let open: Option<(i64,)> =
            sqlx::query_as("SELECT 1 FROM sessions WHERE id = ? AND state != 'closed'")
                .bind(session_id.to_string())
                .fetch_optional(&mut *tx)
                .await?;
        if open.is_none() {
            return Err(ApprovalError::SessionClosed);
        }
        let state = if auto_approve.is_some() {
            ApprovalState::Approved
        } else {
            ApprovalState::Pending
        };
        // A pending row's scope is a placeholder until `resolve` sets the real
        // one; an auto-approved copy is `once` (only the *original* Run-scoped
        // approval is the reusable pattern).
        sqlx::query(
            "INSERT INTO approvals \
             (id, run_id, action_json, risk_json, capabilities_json, state, scope, \
              resolved_by, requested_at, resolved_at, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'once', ?, ?, ?, ?)",
        )
        .bind(approval_id.to_string())
        .bind(run_id.to_string())
        .bind(&action_json)
        .bind(&risk_json)
        .bind(&capabilities_json)
        .bind(state.as_db())
        .bind(auto_approve.as_ref().map(|a| a.resolved_by()))
        .bind(&now_str)
        .bind(if auto_approve.is_some() {
            Some(&now_str)
        } else {
            None
        })
        .bind(&expires_str)
        .execute(&mut *tx)
        .await?;

        let learnable_pattern = match &action {
            ProposedAction::ExecuteCommand {
                program,
                args,
                environment,
                ..
            } => crate::policy::command_pattern(program, args, environment),
            _ => None,
        };

        // ApprovalRequested is always recorded (RULE: persist before publish).
        let requested_seq = next_sequence(&mut *tx, session_id).await?;
        // Derived before `action`/`risk` are moved into the event body; the inbox
        // producer below needs them and the event owns them afterwards.
        let inbox_title = format!("Approval requested: {}", action_kind(&action));
        let inbox_summary = format!("Risk: {:?}", risk.level);
        let requested = EventBody::ApprovalRequested {
            approval_id,
            action,
            risk,
            pattern: learnable_pattern,
        };
        append_event(
            &mut *tx,
            session_id,
            requested_seq,
            &Actor::System,
            &requested,
            &now_str,
        )
        .await?;

        // On auto-approval the resolution is recorded in the same transaction.
        let resolved = if auto_approve.is_some() {
            let resolved_seq = next_sequence(&mut *tx, session_id).await?;
            let body = EventBody::ApprovalResolved {
                approval_id,
                decision: ApprovalDecision::Approve,
            };
            append_event(
                &mut *tx,
                session_id,
                resolved_seq,
                &Actor::System,
                &body,
                &now_str,
            )
            .await?;
            Some((resolved_seq, body))
        } else {
            let session_meta: Option<(Option<i64>, Option<String>)> =
                sqlx::query_as("SELECT owner_uid, repository_id FROM sessions WHERE id = ?")
                    .bind(session_id.to_string())
                    .fetch_optional(&mut *tx)
                    .await?;
            // `migrations/0042_inbox.sql` states the contract for both columns:
            // `owner_uid` is NOT NULL because "there are no pre-migration rows to
            // adopt, so NULL is a bug", and a producer with no repository context
            // "must resolve one before writing rather than inventing a placeholder".
            // Defaulting owner to 0 would file the entry against uid 0 — every inbox
            // read is `WHERE owner_uid = <principal>`, so the real owner could never
            // see or dismiss it while uid 0 could. Skip the projection instead: the
            // approval itself is already durably recorded above, so nothing is lost
            // but the notification.
            let resolved_meta = session_meta.and_then(|(owner_uid, repo_id)| {
                let owner_uid = owner_uid.and_then(|u| u32::try_from(u).ok())?;
                let repository_id = repo_id.and_then(|r| r.parse().ok())?;
                Some((owner_uid, repository_id))
            });
            if let Some((owner_uid, repository_id)) = resolved_meta {
                let title = inbox_title;
                let summary = inbox_summary;
                let _ = crate::inbox::produce_approval_request(
                    &mut tx,
                    owner_uid,
                    repository_id,
                    session_id,
                    run_id,
                    approval_id,
                    title,
                    summary,
                    now,
                )
                .await;
            }
            None
        };

        if auto_approve.is_some() {
            enqueue_control_plane_decision(&mut tx, approval_id).await?;
        }

        tx.commit().await?;

        // Register the waiter BEFORE publishing. Publishing `ApprovalRequested`
        // can make a live controller resolve the approval immediately; if that
        // `resolve()` ran before the waiter existed, its `wake()` would land on
        // nothing and the runtime's later `await_decision()` would park forever.
        // Pre-resolved for auto-approval so `await_decision` returns without a
        // human step; empty (parked) otherwise.
        let initial = auto_approve.map(|_| ApprovalDecision::Approve);
        self.register_waiter(approval_id, initial).await;

        // Persist before publish: only *after* the commit do the lifecycle events
        // fan out to attached clients — mirroring the agent loop's own
        // persist-then-publish for `ToolProposed`. A live controller's approval
        // queue is built from `ApprovalRequested`, so without this a parked run is
        // invisible until re-attach. When no hub is bound (executor-less server,
        // tests) this is a no-op; the durable events above are unaffected.
        if let Some(hub) = &self.subscriptions {
            // The durable events are already committed; a (never-expected) negative
            // sequence must not wrap into a bogus on-wire value, so publish only on
            // a lossless conversion and otherwise skip (the client re-syncs on
            // re-attach catch-up).
            if let Ok(sequence) = u64::try_from(requested_seq) {
                hub.publish(
                    session_id,
                    SessionEvent {
                        sequence,
                        occurred_at: now,
                        causation_id: None,
                        correlation_id: None,
                        actor: Actor::System,
                        body: requested,
                    },
                );
            }
            if let Some((resolved_seq, body)) = resolved {
                if let Ok(sequence) = u64::try_from(resolved_seq) {
                    hub.publish(
                        session_id,
                        SessionEvent {
                            sequence,
                            occurred_at: now,
                            causation_id: None,
                            correlation_id: None,
                            actor: Actor::System,
                            body,
                        },
                    );
                }
            }
        }

        Ok(approval_id)
    }

    /// Block until this approval is resolved, returning the decision.
    ///
    /// Single-consumer: the parked run calls this once. It reads purely from the
    /// waiter registry (no DB round-trip on the hot path); a decision delivered
    /// before the call is still observed because `watch` retains it. Returns
    /// [`ApprovalError::NotFound`] if no waiter is registered (e.g. the id is
    /// unknown, or a restart happened without a preceding
    /// [`reload_pending`](Self::reload_pending)).
    pub async fn await_decision(
        &self,
        approval_id: ApprovalId,
    ) -> Result<ApprovalDecision, ApprovalError> {
        let mut rx = {
            let guard = lock_recovering(&self.waiters);
            match guard.get(&approval_id) {
                Some(sender) => sender.subscribe(),
                None => return Err(ApprovalError::NotFound { approval_id }),
            }
        };

        loop {
            // Copy the retained value out and drop the borrow guard *before* any
            // await. Checking before parking means a decision that landed before
            // subscription is never missed.
            let current = *rx.borrow_and_update();
            if let Some(decision) = current {
                lock_recovering(&self.waiters).remove(&approval_id);
                return Ok(decision);
            }
            if rx.changed().await.is_err() {
                lock_recovering(&self.waiters).remove(&approval_id);
                return Err(ApprovalError::WaiterGone { approval_id });
            }
        }
    }

    /// Resolve a `pending` approval: update the row (`approved`/`rejected`,
    /// `resolved_by`, `resolved_at`, and the real `scope`), append
    /// `ApprovalResolved`, then wake the parked waiter. `Run` scope leaves an
    /// approved row that [`request`](Self::request) treats as an auto-approval
    /// pattern for identical later proposals; `Once` does not.
    ///
    /// Returns the appended `ApprovalResolved` [`SessionEvent`] so a caller can
    /// publish *exactly* it (issue #6 item 2). A standalone entry point (crash
    /// resume, tests); the command write path drives [`resolve_in_tx`] inside its
    /// own transaction so the append is atomic with the command's revision guard.
    pub async fn resolve(
        &self,
        pool: &SqlitePool,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
        scope: ApprovalScope,
        resolved_by: String,
    ) -> Result<SessionEvent, ApprovalError> {
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let event = self
            .resolve_in_tx(
                &mut tx,
                approval_id,
                decision,
                scope,
                resolved_by,
                Utc::now(),
            )
            .await?;
        tx.commit().await?;
        self.wake(approval_id, decision).await;
        Ok(event)
    }

    /// Flip a pending approval and append its `ApprovalResolved` event **inside
    /// the caller's transaction**, returning that exact event. Does NOT commit,
    /// bump any session revision, or wake the waiter — the caller owns the
    /// transaction boundary (so the append is atomic with, e.g., the command
    /// write path's `expected_revision` check and bump) and performs the
    /// post-commit wake/publish. `now` is a parameter so the caller can share one
    /// timestamp across the whole command.
    pub(crate) async fn resolve_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
        scope: ApprovalScope,
        resolved_by: String,
        now: DateTime<Utc>,
    ) -> Result<SessionEvent, ApprovalError> {
        let state = decision_state(decision)?;
        let scope_db = scope_to_db(scope)?;
        let now_str = now.to_rfc3339();

        let existing: Option<(String, String, String, String)> = sqlx::query_as(
            "SELECT a.state, r.session_id, a.action_json, a.run_id FROM approvals a \
             JOIN runs r ON a.run_id = r.id WHERE a.id = ?",
        )
        .bind(approval_id.to_string())
        .fetch_optional(&mut **tx)
        .await?;
        let (current_state, session_id, action_json, run_id) =
            existing.ok_or(ApprovalError::NotFound { approval_id })?;
        if current_state != "pending" {
            return Err(ApprovalError::AlreadyResolved {
                approval_id,
                state: current_state,
            });
        }
        let session_id = parse_session_id(&session_id)?;

        let mut computed_pattern: Option<String> = None;
        if decision == ApprovalDecision::Approve
            && matches!(scope, ApprovalScope::Pattern | ApprovalScope::Repository)
        {
            let action: ProposedAction = serde_json::from_str(&action_json)?;
            let pattern = match &action {
                ProposedAction::ExecuteCommand {
                    program,
                    args,
                    environment,
                    ..
                } => crate::policy::command_pattern(program, args, environment),
                _ => None,
            };
            let pattern = pattern.ok_or(ApprovalError::PatternUnavailable)?;
            computed_pattern = Some(pattern.clone());

            if scope == ApprovalScope::Repository {
                let provenance = crate::commands::run_launch_provenance_in(
                    tx,
                    parse_run_id(&run_id)?,
                    session_id,
                )
                .await
                .map_err(|error| ApprovalError::Corrupt(format!("run repository: {error}")))?;
                let repo = provenance
                    .repository
                    .filter(|r| !r.is_empty())
                    .ok_or(ApprovalError::PatternUnavailable)?;
                let rule_id = uuid::Uuid::now_v7().to_string();
                sqlx::query(
                    "INSERT INTO approval_rules \
                     (id, repository, kind, pattern, created_from_approval, created_by, created_at) \
                     VALUES (?, ?, 'command-prefix', ?, ?, ?, ?)",
                )
                .bind(&rule_id)
                .bind(&repo)
                .bind(&pattern)
                .bind(approval_id.to_string())
                .bind(&resolved_by)
                .bind(&now_str)
                .execute(&mut **tx)
                .await?;
            }
        }

        let updated = sqlx::query(
            "UPDATE approvals SET state = ?, scope = ?, pattern = ?, resolved_by = ?, resolved_at = ? \
             WHERE id = ? AND state = 'pending'",
        )
        .bind(state.as_db())
        .bind(scope_db)
        .bind(computed_pattern)
        .bind(&resolved_by)
        .bind(&now_str)
        .bind(approval_id.to_string())
        .execute(&mut **tx)
        .await?;
        // Lost the race to another resolver / an expiry between our read and here.
        if updated.rows_affected() != 1 {
            return Err(ApprovalError::AlreadyResolved {
                approval_id,
                state: "resolved".to_string(),
            });
        }

        let seq = next_sequence(&mut **tx, session_id).await?;
        let actor = Actor::Human {
            user_id: UserId(resolved_by),
        };
        let body = EventBody::ApprovalResolved {
            approval_id,
            decision,
        };
        append_event(&mut **tx, session_id, seq, &actor, &body, &now_str).await?;
        let _ = crate::inbox::resolve_approval_entry(tx, approval_id, now).await;
        enqueue_control_plane_decision(tx, approval_id).await?;

        Ok(SessionEvent {
            sequence: u64::try_from(seq)
                .map_err(|_| ApprovalError::Corrupt(format!("negative event sequence {seq}")))?,
            occurred_at: now,
            causation_id: None,
            correlation_id: None,
            actor,
            body,
        })
    }

    /// Expire every `pending` approval whose `expires_at` is at or before `now`:
    /// mark it `expired`, append `ApprovalResolved { Reject }` (an expiry behaves
    /// as a rejection), and wake its waiter with `Reject`. Returns how many were
    /// expired. `now` is a parameter so a daemon task can drive it and tests can
    /// pin it.
    pub async fn expire_due(
        &self,
        pool: &SqlitePool,
        now: DateTime<Utc>,
    ) -> Result<usize, ApprovalError> {
        // Load candidates and compare instants in Rust rather than trusting
        // lexicographic timestamp ordering in SQL.
        let candidates: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT a.id, r.session_id, a.expires_at FROM approvals a \
             JOIN runs r ON a.run_id = r.id \
             WHERE a.state = 'pending' AND a.expires_at IS NOT NULL",
        )
        .fetch_all(pool)
        .await?;

        let mut expired = 0usize;
        for (id_str, session_str, expires_str) in candidates {
            let Some(expires_str) = expires_str else {
                continue;
            };
            let expires_at = parse_ts(&expires_str, "expires_at")?;
            if expires_at > now {
                continue;
            }
            let approval_id = parse_approval_id(&id_str)?;
            let session_id = parse_session_id(&session_str)?;
            let now_str = now.to_rfc3339();

            let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
            let updated = sqlx::query(
                "UPDATE approvals SET state = 'expired', resolved_at = ? \
                 WHERE id = ? AND state = 'pending'",
            )
            .bind(&now_str)
            .bind(approval_id.to_string())
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() != 1 {
                // Resolved concurrently; skip.
                continue;
            }
            let seq = next_sequence(&mut *tx, session_id).await?;
            append_event(
                &mut *tx,
                session_id,
                seq,
                &Actor::System,
                &EventBody::ApprovalResolved {
                    approval_id,
                    decision: ApprovalDecision::Reject,
                },
                &now_str,
            )
            .await?;
            enqueue_control_plane_decision(&mut tx, approval_id).await?;
            tx.commit().await?;

            self.wake(approval_id, ApprovalDecision::Reject).await;
            self.publish_resolved(session_id, seq, now, approval_id);
            expired += 1;
        }
        Ok(expired)
    }

    /// Expire every `pending` approval whose run has already reached a terminal
    /// state — the run can never consume the decision, so leaving the row
    /// `pending` re-surfaces a dead request on every boot, forever. Used by
    /// startup recovery after it fails the live runs. Marks each `expired` and
    /// appends `ApprovalResolved { Reject }` exactly as a deadline expiry does.
    pub async fn expire_orphaned(
        &self,
        pool: &SqlitePool,
        now: DateTime<Utc>,
    ) -> Result<Vec<ApprovalId>, ApprovalError> {
        let candidates: Vec<(String, String)> = sqlx::query_as(
            "SELECT a.id, r.session_id FROM approvals a \
             JOIN runs r ON a.run_id = r.id \
             WHERE a.state = 'pending' \
               AND r.state IN ('Completed', 'Failed', 'Cancelled')",
        )
        .fetch_all(pool)
        .await?;

        let mut expired = Vec::new();
        for (id_str, session_str) in candidates {
            let approval_id = parse_approval_id(&id_str)?;
            let session_id = parse_session_id(&session_str)?;
            let now_str = now.to_rfc3339();

            let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
            let updated = sqlx::query(
                "UPDATE approvals SET state = 'expired', resolved_at = ? \
                 WHERE id = ? AND state = 'pending'",
            )
            .bind(&now_str)
            .bind(approval_id.to_string())
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() != 1 {
                continue;
            }
            let seq = next_sequence(&mut *tx, session_id).await?;
            append_event(
                &mut *tx,
                session_id,
                seq,
                &Actor::System,
                &EventBody::ApprovalResolved {
                    approval_id,
                    decision: ApprovalDecision::Reject,
                },
                &now_str,
            )
            .await?;
            enqueue_control_plane_decision(&mut tx, approval_id).await?;
            tx.commit().await?;

            self.wake(approval_id, ApprovalDecision::Reject).await;
            self.publish_resolved(session_id, seq, now, approval_id);
            expired.push(approval_id);
        }
        Ok(expired)
    }

    /// Publish an `ApprovalResolved { Reject }` produced by an expiry to any
    /// live subscribers (persist-before-publish: callers commit first). Without
    /// this, an expiry is durable but invisible until the client re-attaches.
    fn publish_resolved(
        &self,
        session_id: SessionId,
        seq: i64,
        now: DateTime<Utc>,
        approval_id: ApprovalId,
    ) {
        if let Some(hub) = &self.subscriptions {
            if let Ok(sequence) = u64::try_from(seq) {
                hub.publish(
                    session_id,
                    SessionEvent {
                        sequence,
                        occurred_at: now,
                        causation_id: None,
                        correlation_id: None,
                        actor: Actor::System,
                        body: EventBody::ApprovalResolved {
                            approval_id,
                            decision: ApprovalDecision::Reject,
                        },
                    },
                );
            }
        }
    }

    /// Re-load every `pending` approval on daemon restart and re-register a
    /// waiter for each, so newly attached clients can re-surface the request and
    /// a resuming run can [`await_decision`](Self::await_decision) again. Nothing
    /// is lost across a restart.
    pub async fn reload_pending(
        &self,
        pool: &SqlitePool,
    ) -> Result<Vec<PendingApproval>, ApprovalError> {
        let rows: Vec<PendingRow> = sqlx::query_as(
            "SELECT a.id, a.run_id, r.session_id, a.action_json, a.risk_json, \
                    a.capabilities_json, a.requested_at, a.expires_at \
             FROM approvals a JOIN runs r ON a.run_id = r.id \
             WHERE a.state = 'pending'",
        )
        .fetch_all(pool)
        .await?;

        let mut pending = Vec::with_capacity(rows.len());
        for row in rows {
            let approval = pending_from_row(row)?;
            // Re-register only if a waiter is not already live (idempotent
            // reload).
            let mut guard = lock_recovering(&self.waiters);
            guard
                .entry(approval.approval_id)
                .or_insert_with(|| watch::channel(None).0);
            drop(guard);
            pending.push(approval);
        }
        Ok(pending)
    }

    /// Insert a waiter for `approval_id`, optionally pre-loaded with a decision
    /// (auto-approval). Never *replaces* an existing entry: a resolution racing
    /// in between the request's commit and this registration pre-creates the
    /// waiter with its decision (see [`wake`](Self::wake)), and clobbering it
    /// would drop that decision and park the run forever.
    async fn register_waiter(&self, approval_id: ApprovalId, initial: Option<ApprovalDecision>) {
        let mut guard = lock_recovering(&self.waiters);
        let sender = guard
            .entry(approval_id)
            .or_insert_with(|| watch::channel(None).0);
        if let Some(decision) = initial {
            sender.send_replace(Some(decision));
        }
    }

    /// Deliver `decision` to a parked waiter. If none is registered yet — a
    /// client that learned the approval id from the durable `ApprovalRequested`
    /// event can resolve it in the window between the request's commit and its
    /// waiter registration — the waiter is created pre-resolved so the decision
    /// is retained for the runtime's later `await_decision`. `send_replace`
    /// never fails even when nobody is subscribed yet.
    pub(crate) async fn wake(&self, approval_id: ApprovalId, decision: ApprovalDecision) {
        let mut guard = lock_recovering(&self.waiters);
        guard
            .entry(approval_id)
            .or_insert_with(|| watch::channel(None).0)
            .send_replace(Some(decision));
    }

    /// Drop the waiter for `approval_id`, if any. Called when the run that was
    /// parked on this approval is cancelled — the `await_decision` future is
    /// dropped without consuming the entry, which would otherwise leak for the
    /// daemon's lifetime.
    pub fn forget_waiter(&self, approval_id: ApprovalId) {
        lock_recovering(&self.waiters).remove(&approval_id);
    }

    /// Whether an identical action (by [`action_digest`]) was already approved
    /// `Run`-scoped in this run — the auto-approval check.
    async fn run_scoped_match(
        &self,
        pool: &SqlitePool,
        run_id: RunId,
        digest: &str,
    ) -> Result<bool, ApprovalError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT action_json FROM approvals \
             WHERE run_id = ? AND scope = 'run' AND state = 'approved'",
        )
        .bind(run_id.to_string())
        .fetch_all(pool)
        .await?;
        Ok(rows
            .into_iter()
            .any(|(action_json,)| digest_of(action_json.as_bytes()) == digest))
    }

    /// Prefix-pattern reuse for ExecuteCommand actions ONLY. Both the learned
    /// pattern and the candidate must be learnable (`command_pattern` returns
    /// Some for the candidate) — an interpreter call or an env-carrying call
    /// can never be covered, even by a rule that would textually match.
    async fn pattern_match(
        &self,
        pool: &SqlitePool,
        run_id: RunId,
        repository: Option<&str>,
        action: &ProposedAction,
    ) -> Result<Option<AutoApproval>, ApprovalError> {
        let ProposedAction::ExecuteCommand {
            program,
            args,
            environment,
            ..
        } = action
        else {
            return Ok(None);
        };
        // Candidate must itself be learnable — this re-checks the environment
        // and interpreter rules on the CANDIDATE, not just on the learned side.
        if crate::policy::command_pattern(program, args, environment).is_none() {
            return Ok(None);
        }
        // (1) run-scoped Pattern approvals.
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT pattern FROM approvals \
             WHERE run_id = ? AND scope = 'pattern' AND state = 'approved' \
               AND pattern IS NOT NULL",
        )
        .bind(run_id.to_string())
        .fetch_all(pool)
        .await?;
        for (pattern,) in rows {
            if crate::policy::pattern_matches(&pattern, program, args) {
                return Ok(Some(AutoApproval::RunPattern { pattern }));
            }
        }
        // (2) persisted repository rules.
        if let Some(repository) = repository {
            let rules: Vec<(String, String)> = sqlx::query_as(
                "SELECT id, pattern FROM approval_rules \
                 WHERE repository = ? AND kind = 'command-prefix' AND revoked_at IS NULL",
            )
            .bind(repository)
            .fetch_all(pool)
            .await?;
            for (rule_id, pattern) in rules {
                if crate::policy::pattern_matches(&pattern, program, args) {
                    return Ok(Some(AutoApproval::RepositoryRule { rule_id, pattern }));
                }
            }
        }
        Ok(None)
    }
}

/// A short kind label for a proposed action, used for the inbox entry title.
///
/// [`ProposedAction`] is `#[non_exhaustive]`, so the catch-all is required. It is
/// *display only* — nothing downstream branches on this string — but it still
/// fails closed by naming the action "unsupported" rather than guessing a
/// friendlier label for a variant this daemon cannot describe.
fn action_kind(action: &ProposedAction) -> &'static str {
    match action {
        ProposedAction::ReadFiles { .. } => "read files",
        ProposedAction::WritePatch { .. } => "apply patch",
        ProposedAction::ExecuteCommand { .. } => "run command",
        ProposedAction::NetworkRequest { .. } => "network",
        ProposedAction::GitCommit { .. } => "git commit",
        ProposedAction::GitPush { .. } => "git push",
        ProposedAction::GitHubMutation { .. } => "github mutation",
        ProposedAction::PublishDocument { .. } => "publish document",
        ProposedAction::BlackboardPost { .. } => "blackboard post",
        ProposedAction::BlackboardQuery { .. } => "blackboard query",
        ProposedAction::McpToolCall { .. } => "mcp tool",
        ProposedAction::AcpToolCall { .. } => "acp tool",
        ProposedAction::DocumentEdit { .. } => "edit document",
        ProposedAction::WorkflowQuery { .. } => "query workflow",
        ProposedAction::WorkflowCreate { .. } => "create workflow",
        ProposedAction::WorkflowRun { .. } => "run workflow",
        ProposedAction::TaskWrite { .. } => "write task",
        ProposedAction::TaskRead { .. } => "read task",
        ProposedAction::CouncilCreate { .. } => "create council",
        ProposedAction::CouncilRun { .. } => "run council",
        ProposedAction::CouncilResultRead { .. } => "read council result",
        ProposedAction::CodeGraphQuery { .. } => "code graph query",
        ProposedAction::CodeGraphAssert { .. } => "code graph assert",
        ProposedAction::AskUser { .. } => "ask user",
        ProposedAction::RestoreCheckpoint { .. } => "restore checkpoint",
        ProposedAction::WriteProcessStdin { .. } => "write process stdin",
        ProposedAction::PlanTransition { .. } => "plan transition",
        ProposedAction::ReadSecret { .. } => "read secret",
        ProposedAction::Unknown | _ => "unsupported",
    }
}

/// The hex SHA-256 of a proposed action's canonical JSON serialization — the
/// per-run auto-approval matching key. Two structurally identical actions
/// produce the same digest.
pub fn action_digest(action: &ProposedAction) -> Result<String, ApprovalError> {
    Ok(digest_of(serde_json::to_string(action)?.as_bytes()))
}

fn digest_of(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn decision_state(decision: ApprovalDecision) -> Result<ApprovalState, ApprovalError> {
    match decision {
        ApprovalDecision::Approve => Ok(ApprovalState::Approved),
        ApprovalDecision::Reject => Ok(ApprovalState::Rejected),
        _ => Err(ApprovalError::UnsupportedDecision),
    }
}

fn scope_to_db(scope: ApprovalScope) -> Result<&'static str, ApprovalError> {
    match scope {
        ApprovalScope::Once => Ok("once"),
        ApprovalScope::Run => Ok("run"),
        ApprovalScope::Pattern => Ok("pattern"),
        ApprovalScope::Repository => Ok("repository"),
        _ => Err(ApprovalError::UnsupportedScope),
    }
}

/// The next event sequence for a session (1-based), read inside the caller's
/// transaction so the append that claims it is atomic with the read.
async fn next_sequence(
    executor: impl sqlx::SqliteExecutor<'_>,
    session_id: SessionId,
) -> Result<i64, ApprovalError> {
    let (max,): (i64,) =
        sqlx::query_as("SELECT COALESCE(MAX(sequence), 0) FROM events WHERE session_id = ?")
            .bind(session_id.to_string())
            .fetch_one(executor)
            .await?;
    Ok(max + 1)
}

/// Append one event within the caller's transaction. Mirrors
/// [`crate::ledger::append_event`] but binds against a transaction (the ledger
/// helper takes the pool) so the sequence/append pair is atomic.
async fn append_event(
    executor: impl sqlx::SqliteExecutor<'_>,
    session_id: SessionId,
    sequence: i64,
    actor: &Actor,
    body: &EventBody,
    occurred_at: &str,
) -> Result<(), ApprovalError> {
    sqlx::query(
        "INSERT INTO events \
         (session_id, sequence, occurred_at, actor, body, causation_id, correlation_id, schema_version) \
         VALUES (?, ?, ?, ?, ?, NULL, NULL, 1)",
    )
    .bind(session_id.to_string())
    .bind(sequence)
    .bind(occurred_at)
    .bind(serde_json::to_string(actor)?)
    .bind(serde_json::to_string(body)?)
    .execute(executor)
    .await?;
    Ok(())
}

/// Row shape returned by [`ApprovalBroker::reload_pending`]:
/// (id, run_id, session_id, action_json, risk_json, capabilities_json,
/// requested_at, expires_at).
type PendingRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
);

fn pending_from_row(row: PendingRow) -> Result<PendingApproval, ApprovalError> {
    let (
        id,
        run_id,
        session_id,
        action_json,
        risk_json,
        capabilities_json,
        requested_at,
        expires_at,
    ) = row;
    Ok(PendingApproval {
        approval_id: parse_approval_id(&id)?,
        run_id: parse_run_id(&run_id)?,
        session_id: parse_session_id(&session_id)?,
        action: serde_json::from_str(&action_json)?,
        risk: serde_json::from_str(&risk_json)?,
        capabilities: serde_json::from_str(&capabilities_json)?,
        requested_at: parse_ts(&requested_at, "requested_at")?,
        expires_at: expires_at.map(|t| parse_ts(&t, "expires_at")).transpose()?,
    })
}

fn parse_approval_id(s: &str) -> Result<ApprovalId, ApprovalError> {
    ApprovalId::from_str(s).map_err(|e| ApprovalError::Corrupt(format!("approval id: {e}")))
}

fn parse_run_id(s: &str) -> Result<RunId, ApprovalError> {
    RunId::from_str(s).map_err(|e| ApprovalError::Corrupt(format!("run id: {e}")))
}

fn parse_session_id(s: &str) -> Result<SessionId, ApprovalError> {
    SessionId::from_str(s).map_err(|e| ApprovalError::Corrupt(format!("session id: {e}")))
}

fn parse_ts(s: &str, field: &str) -> Result<DateTime<Utc>, ApprovalError> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| ApprovalError::Corrupt(format!("{field}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::RiskLevel;
    use std::path::Path;
    use tempfile::tempdir;

    async fn test_pool(dir: &Path) -> SqlitePool {
        crate::db::open_database(&dir.join("test.db"))
            .await
            .expect("open database")
    }

    async fn seed_session_run(pool: &SqlitePool) -> (SessionId, RunId) {
        seed_session_run_with_repo(pool, Some("/home/user/repo")).await
    }

    async fn seed_session_run_with_repo(
        pool: &SqlitePool,
        repository: Option<&str>,
    ) -> (SessionId, RunId) {
        let session_id = SessionId::new();
        crate::ledger::create_session(pool, session_id, "approval-test")
            .await
            .expect("create session");

        let run_id = RunId::new();
        sqlx::query(
            "INSERT INTO runs (id, session_id, objective, state, mode, model_policy, budget_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(run_id.to_string())
        .bind(session_id.to_string())
        .bind("diagnose")
        .bind("Running")
        .bind("Build")
        .bind("hosted-default")
        .bind("{}")
        .execute(pool)
        .await
        .expect("insert run");

        if let Some(repo) = repository {
            let body = serde_json::json!({
                "type": "StartRun",
                "session_id": session_id,
                "objective": "diagnose",
                "repository": repo,
                "mode": { "type": "Build" },
            });
            sqlx::query(
                "INSERT INTO commands (id, idempotency_key, session_id, client_id, body, status, result_json, received_at) \
                 VALUES (?, ?, ?, 'test-client', ?, 'applied', ?, ?)",
            )
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(session_id.to_string())
            .bind(body.to_string())
            .bind(serde_json::json!({ "created_run": run_id }).to_string())
            .bind(chrono::Utc::now().to_rfc3339())
            .execute(pool)
            .await
            .expect("insert start run command");
        }

        (session_id, run_id)
    }

    fn sample_action() -> ProposedAction {
        ProposedAction::ExecuteCommand {
            program: "cargo".to_string(),
            args: vec!["test".to_string()],
            environment: Vec::new(),
            cwd: None,
        }
    }

    fn sample_risk() -> Risk {
        Risk {
            level: RiskLevel::Medium,
            reasons: vec!["runs a shell command".to_string()],
        }
    }

    async fn state_of(pool: &SqlitePool, id: ApprovalId) -> String {
        let (state,): (String,) = sqlx::query_as("SELECT state FROM approvals WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(pool)
            .await
            .expect("fetch approval state");
        state
    }

    /// Whether an `ApprovalResolved` event for `id` with `decision` exists in the
    /// session ledger.
    async fn resolved_event_exists(
        pool: &SqlitePool,
        session_id: SessionId,
        id: ApprovalId,
        decision: ApprovalDecision,
    ) -> bool {
        let events = crate::ledger::load_events(pool, session_id)
            .await
            .expect("load events");
        events.iter().any(|e| {
            matches!(
                &e.body,
                EventBody::ApprovalResolved { approval_id, decision: d }
                    if *approval_id == id && *d == decision
            )
        })
    }

    async fn requested_event_exists(
        pool: &SqlitePool,
        session_id: SessionId,
        id: ApprovalId,
    ) -> bool {
        let events = crate::ledger::load_events(pool, session_id)
            .await
            .expect("load events");
        events.iter().any(|e| {
            matches!(&e.body, EventBody::ApprovalRequested { approval_id, .. } if *approval_id == id)
        })
    }

    #[tokio::test]
    async fn request_cannot_append_to_a_closed_session() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let (session_id, run_id) = seed_session_run(&pool).await;
        sqlx::query("UPDATE sessions SET state = 'closed' WHERE id = ?")
            .bind(session_id.to_string())
            .execute(&pool)
            .await
            .unwrap();

        let error = ApprovalBroker::new()
            .request(
                &pool,
                session_id,
                run_id,
                None,
                sample_action(),
                sample_risk(),
                Vec::new(),
                None,
            )
            .await
            .expect_err("closed sessions reject new approval work");
        assert!(matches!(error, ApprovalError::SessionClosed));
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM approvals WHERE run_id = ?")
            .bind(run_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn approve_round_trip_wakes_the_waiter() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let (session, run) = seed_session_run(&pool).await;
        let broker = ApprovalBroker::new();

        let id = broker
            .request(
                &pool,
                session,
                run,
                None,
                sample_action(),
                sample_risk(),
                vec![Capability::GitCommit],
                None,
            )
            .await
            .unwrap();
        assert_eq!(state_of(&pool, id).await, "pending");
        assert!(requested_event_exists(&pool, session, id).await);

        // Park an awaiter, then resolve from another clone.
        let awaiter = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.await_decision(id).await })
        };
        broker
            .resolve(
                &pool,
                id,
                ApprovalDecision::Approve,
                ApprovalScope::Once,
                "tester".to_string(),
            )
            .await
            .unwrap();

        let decision = awaiter.await.unwrap().unwrap();
        assert_eq!(decision, ApprovalDecision::Approve);
        assert_eq!(state_of(&pool, id).await, "approved");
        assert!(resolved_event_exists(&pool, session, id, ApprovalDecision::Approve).await);
    }

    #[tokio::test]
    async fn reject_round_trip_wakes_the_waiter() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let (session, run) = seed_session_run(&pool).await;
        let broker = ApprovalBroker::new();

        let id = broker
            .request(
                &pool,
                session,
                run,
                None,
                sample_action(),
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();

        let awaiter = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.await_decision(id).await })
        };
        broker
            .resolve(
                &pool,
                id,
                ApprovalDecision::Reject,
                ApprovalScope::Once,
                "tester".to_string(),
            )
            .await
            .unwrap();

        let decision = awaiter.await.unwrap().unwrap();
        assert_eq!(decision, ApprovalDecision::Reject);
        assert_eq!(state_of(&pool, id).await, "rejected");
        assert!(resolved_event_exists(&pool, session, id, ApprovalDecision::Reject).await);
    }

    #[tokio::test]
    async fn run_scoped_resolution_auto_approves_identical_proposal() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let (session, run) = seed_session_run(&pool).await;
        let broker = ApprovalBroker::new();

        // First proposal: resolve Run-scoped -> records the pattern.
        let first = broker
            .request(
                &pool,
                session,
                run,
                None,
                sample_action(),
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();
        broker
            .resolve(
                &pool,
                first,
                ApprovalDecision::Approve,
                ApprovalScope::Run,
                "tester".to_string(),
            )
            .await
            .unwrap();

        // Second, identical proposal: auto-approved on request, no parking.
        let second = broker
            .request(
                &pool,
                session,
                run,
                None,
                sample_action(),
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(state_of(&pool, second).await, "approved");

        // The parked run observes Approve immediately.
        let decision = broker.await_decision(second).await.unwrap();
        assert_eq!(decision, ApprovalDecision::Approve);
        // Auditable: both events exist for the auto-approved id.
        assert!(requested_event_exists(&pool, session, second).await);
        assert!(resolved_event_exists(&pool, session, second, ApprovalDecision::Approve).await);

        // A *different* action is not auto-approved.
        let other = broker
            .request(
                &pool,
                session,
                run,
                None,
                ProposedAction::ExecuteCommand {
                    program: "cargo".to_string(),
                    args: vec!["build".to_string()],
                    environment: Vec::new(),
                    cwd: None,
                },
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();
        assert_eq!(state_of(&pool, other).await, "pending");
    }

    #[tokio::test]
    async fn restart_re_surfaces_pending_and_still_resolves() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let (session, run) = seed_session_run(&pool).await;

        // A broker parks a request, then "crashes" (dropped).
        let id = {
            let broker = ApprovalBroker::new();
            broker
                .request(
                    &pool,
                    session,
                    run,
                    None,
                    sample_action(),
                    sample_risk(),
                    vec![Capability::GitCommit],
                    None,
                )
                .await
                .unwrap()
        };

        // A fresh broker over the same pool re-surfaces and re-registers it.
        let broker = ApprovalBroker::new();
        let pending = broker.reload_pending(&pool).await.unwrap();
        let surfaced = pending
            .iter()
            .find(|p| p.approval_id == id)
            .expect("pending approval re-surfaced");
        assert_eq!(surfaced.run_id, run);
        assert_eq!(surfaced.session_id, session);
        assert_eq!(surfaced.capabilities, vec![Capability::GitCommit]);

        // The re-registered waiter can still be awaited and resolved.
        let awaiter = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.await_decision(id).await })
        };
        broker
            .resolve(
                &pool,
                id,
                ApprovalDecision::Approve,
                ApprovalScope::Once,
                "tester".to_string(),
            )
            .await
            .unwrap();
        assert_eq!(awaiter.await.unwrap().unwrap(), ApprovalDecision::Approve);
        assert_eq!(state_of(&pool, id).await, "approved");
    }

    #[tokio::test]
    async fn expiry_marks_expired_and_rejects_the_waiter() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let (session, run) = seed_session_run(&pool).await;
        let broker = ApprovalBroker::new();

        let past = Utc::now() - chrono::Duration::seconds(60);
        let id = broker
            .request(
                &pool,
                session,
                run,
                None,
                sample_action(),
                sample_risk(),
                vec![],
                Some(past),
            )
            .await
            .unwrap();

        let awaiter = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.await_decision(id).await })
        };

        let expired = broker.expire_due(&pool, Utc::now()).await.unwrap();
        assert_eq!(expired, 1);
        assert_eq!(state_of(&pool, id).await, "expired");

        // Expiry behaves as a rejection for the parked run.
        let decision = awaiter.await.unwrap().unwrap();
        assert_eq!(decision, ApprovalDecision::Reject);
        assert!(resolved_event_exists(&pool, session, id, ApprovalDecision::Reject).await);
    }

    /// Poison the waiter map the only way it can be poisoned — a panic while
    /// holding it — and prove the approval path still both delivers a real
    /// decision AND withholds one for an unknown id. With `.expect(...)` back
    /// in place, every call below panics instead of answering.
    #[tokio::test]
    async fn a_poisoned_waiter_map_neither_wedges_nor_fabricates_approval() {
        let broker = ApprovalBroker::new();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = broker.waiters.lock().expect("fresh mutex");
            panic!("a holder panicked");
        }));
        assert!(broker.waiters.is_poisoned());

        // An id nobody woke stays unresolved: absent is still `NotFound`, not
        // an accidental approval.
        let unknown = ApprovalId::new();
        assert!(matches!(
            broker.await_decision(unknown).await,
            Err(ApprovalError::NotFound { .. })
        ));

        // A real decision still reaches its parked waiter.
        let approval_id = ApprovalId::new();
        broker.wake(approval_id, ApprovalDecision::Approve).await;
        assert_eq!(
            broker.await_decision(approval_id).await.unwrap(),
            ApprovalDecision::Approve
        );
        broker.forget_waiter(approval_id);
    }

    #[tokio::test]
    async fn snapshot_preserves_only_daemon_learnable_command_patterns() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let (session, run) = seed_session_run(&pool).await;
        let broker = ApprovalBroker::new();
        for (program, expected) in [("git", Some("git status *")), ("python3", None)] {
            let action = ProposedAction::ExecuteCommand {
                program: program.to_string(),
                args: vec!["status".to_string()],
                environment: vec![],
                cwd: None,
            };
            let id = broker
                .request(
                    &pool,
                    session,
                    run,
                    None,
                    action,
                    sample_risk(),
                    vec![],
                    None,
                )
                .await
                .unwrap();
            let snapshot = crate::projections::session_projection(&pool, session)
                .await
                .unwrap();
            let approval = snapshot
                .pending_approvals
                .iter()
                .find(|approval| approval.approval_id == id)
                .unwrap();
            assert_eq!(approval.pattern.as_deref(), expected);
        }
    }

    #[tokio::test]
    async fn resolving_a_missing_approval_is_not_found() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let broker = ApprovalBroker::new();
        let err = broker
            .resolve(
                &pool,
                ApprovalId::new(),
                ApprovalDecision::Approve,
                ApprovalScope::Once,
                "tester".to_string(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ApprovalError::NotFound { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn double_resolve_reports_already_resolved() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let (session, run) = seed_session_run(&pool).await;
        let broker = ApprovalBroker::new();

        let id = broker
            .request(
                &pool,
                session,
                run,
                None,
                sample_action(),
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();
        broker
            .resolve(
                &pool,
                id,
                ApprovalDecision::Approve,
                ApprovalScope::Once,
                "tester".to_string(),
            )
            .await
            .unwrap();
        let err = broker
            .resolve(
                &pool,
                id,
                ApprovalDecision::Reject,
                ApprovalScope::Once,
                "tester".to_string(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ApprovalError::AlreadyResolved { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn pattern_scoped_resolution_auto_approves_prefix_match() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let (session, run) = seed_session_run(&pool).await;
        let broker = ApprovalBroker::new();

        let action1 = ProposedAction::ExecuteCommand {
            program: "git".to_string(),
            args: vec!["checkout".to_string(), "main".to_string()],
            environment: Vec::new(),
            cwd: None,
        };

        let first = broker
            .request(
                &pool,
                session,
                run,
                Some("/home/user/repo"),
                action1,
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();

        broker
            .resolve(
                &pool,
                first,
                ApprovalDecision::Approve,
                ApprovalScope::Pattern,
                "tester".to_string(),
            )
            .await
            .unwrap();

        // Pattern was stamped in approvals table
        let (pattern,): (Option<String>,) =
            sqlx::query_as("SELECT pattern FROM approvals WHERE id = ?")
                .bind(first.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pattern.as_deref(), Some("git checkout *"));

        // Second proposal with different branch: auto-approves via pattern
        let action2 = ProposedAction::ExecuteCommand {
            program: "git".to_string(),
            args: vec!["checkout".to_string(), "feature/x".to_string()],
            environment: Vec::new(),
            cwd: None,
        };
        let second = broker
            .request(
                &pool,
                session,
                run,
                Some("/home/user/repo"),
                action2,
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();

        assert_eq!(state_of(&pool, second).await, "approved");
        let (resolved_by,): (Option<String>,) =
            sqlx::query_as("SELECT resolved_by FROM approvals WHERE id = ?")
                .bind(second.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(resolved_by.as_deref(), Some("auto:pattern:git checkout *"));

        // Different subcommand: still parks
        let action3 = ProposedAction::ExecuteCommand {
            program: "git".to_string(),
            args: vec!["push".to_string(), "origin".to_string(), "main".to_string()],
            environment: Vec::new(),
            cwd: None,
        };
        let third = broker
            .request(
                &pool,
                session,
                run,
                Some("/home/user/repo"),
                action3,
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();
        assert_eq!(state_of(&pool, third).await, "pending");
    }

    #[tokio::test]
    async fn pattern_never_matches_env_carrying_candidate() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let (session, run) = seed_session_run(&pool).await;
        let broker = ApprovalBroker::new();

        let clean_action = ProposedAction::ExecuteCommand {
            program: "git".to_string(),
            args: vec!["checkout".to_string(), "main".to_string()],
            environment: Vec::new(),
            cwd: None,
        };

        let first = broker
            .request(
                &pool,
                session,
                run,
                Some("/home/user/repo"),
                clean_action,
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();

        broker
            .resolve(
                &pool,
                first,
                ApprovalDecision::Approve,
                ApprovalScope::Pattern,
                "tester".to_string(),
            )
            .await
            .unwrap();

        // Candidate with environment set MUST NOT match pattern
        let env_action = ProposedAction::ExecuteCommand {
            program: "git".to_string(),
            args: vec!["checkout".to_string(), "feature".to_string()],
            environment: vec![("GIT_DIR".to_string(), "/tmp/smuggle".to_string())],
            cwd: None,
        };
        let second = broker
            .request(
                &pool,
                session,
                run,
                Some("/home/user/repo"),
                env_action,
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();
        assert_eq!(state_of(&pool, second).await, "pending");
    }

    #[tokio::test]
    async fn always_approval_disposition_skips_pattern_reuse() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let (session, run) = seed_session_run(&pool).await;
        let broker = ApprovalBroker::new();

        let action = ProposedAction::ExecuteCommand {
            program: "git".to_string(),
            args: vec!["checkout".to_string(), "main".to_string()],
            environment: Vec::new(),
            cwd: None,
        };

        let first = broker
            .request(
                &pool,
                session,
                run,
                Some("/home/user/repo"),
                action.clone(),
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();

        broker
            .resolve(
                &pool,
                first,
                ApprovalDecision::Approve,
                ApprovalScope::Pattern,
                "tester".to_string(),
            )
            .await
            .unwrap();

        // When allow_run_reuse is false (AlwaysApproval disposition), pattern reuse is skipped
        let second = broker
            .request_with_reuse(
                &pool,
                session,
                run,
                Some("/home/user/repo"),
                action,
                sample_risk(),
                vec![],
                None,
                false,
            )
            .await
            .unwrap();
        assert_eq!(state_of(&pool, second).await, "pending");
    }

    #[tokio::test]
    async fn repository_rule_uses_the_approving_runs_origin_not_later_session_commands() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let (session, run) = seed_session_run_with_repo(&pool, Some("/repository-at-origin")).await;
        let broker = ApprovalBroker::new();
        let id = broker
            .request(
                &pool,
                session,
                run,
                None,
                sample_action(),
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO commands (id, idempotency_key, session_id, client_id, body, status, result_json, received_at) \
             SELECT ?, ?, session_id, client_id, json_set(body, '$.repository', '/later-repository'), 'applied', '{}', ? \
             FROM commands WHERE session_id = ? LIMIT 1",
        )
        .bind(uuid::Uuid::now_v7().to_string()).bind(uuid::Uuid::now_v7().to_string())
        .bind((Utc::now() + chrono::Duration::seconds(1)).to_rfc3339()).bind(session.to_string())
        .execute(&pool).await.unwrap();
        broker
            .resolve(
                &pool,
                id,
                ApprovalDecision::Approve,
                ApprovalScope::Repository,
                "tester".to_string(),
            )
            .await
            .unwrap();
        let repositories: Vec<(String,)> = sqlx::query_as("SELECT repository FROM approval_rules")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(repositories, vec![("/repository-at-origin".to_string(),)]);
    }

    #[tokio::test]
    async fn repository_rule_persists_across_runs_and_respects_revocation() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let repo_root = "/home/user/my-special-repo";
        let (session1, run1) = seed_session_run_with_repo(&pool, Some(repo_root)).await;
        let broker = ApprovalBroker::new();

        let action = ProposedAction::ExecuteCommand {
            program: "git".to_string(),
            args: vec!["checkout".to_string(), "main".to_string()],
            environment: Vec::new(),
            cwd: None,
        };

        let first = broker
            .request(
                &pool,
                session1,
                run1,
                Some(repo_root),
                action,
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();

        broker
            .resolve(
                &pool,
                first,
                ApprovalDecision::Approve,
                ApprovalScope::Repository,
                "tester".to_string(),
            )
            .await
            .unwrap();

        // Check approval_rules table
        let rule: (String, String, String, Option<String>) = sqlx::query_as(
            "SELECT id, repository, pattern, revoked_at FROM approval_rules WHERE repository = ?",
        )
        .bind(repo_root)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rule.1, repo_root);
        assert_eq!(rule.2, "git checkout *");
        assert!(rule.3.is_none());

        // A NEW run in the SAME repository auto-approves
        let (session2, run2) = seed_session_run_with_repo(&pool, Some(repo_root)).await;
        let action2 = ProposedAction::ExecuteCommand {
            program: "git".to_string(),
            args: vec!["checkout".to_string(), "other-branch".to_string()],
            environment: Vec::new(),
            cwd: None,
        };
        let second = broker
            .request(
                &pool,
                session2,
                run2,
                Some(repo_root),
                action2.clone(),
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();
        assert_eq!(state_of(&pool, second).await, "approved");
        let (resolved_by,): (Option<String>,) =
            sqlx::query_as("SELECT resolved_by FROM approvals WHERE id = ?")
                .bind(second.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            resolved_by.as_deref(),
            Some(format!("auto:repo-rule:{}", rule.0).as_str())
        );

        // A run in a DIFFERENT repository parks
        let diff_repo = "/home/user/other-repo";
        let (session3, run3) = seed_session_run_with_repo(&pool, Some(diff_repo)).await;
        let third = broker
            .request(
                &pool,
                session3,
                run3,
                Some(diff_repo),
                action2.clone(),
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();
        assert_eq!(state_of(&pool, third).await, "pending");

        // Revoke the rule
        sqlx::query("UPDATE approval_rules SET revoked_at = '2026-08-15T00:00:00Z' WHERE id = ?")
            .bind(&rule.0)
            .execute(&pool)
            .await
            .unwrap();

        // After revocation, request in the original repo parks again
        let (session4, run4) = seed_session_run_with_repo(&pool, Some(repo_root)).await;
        let fourth = broker
            .request(
                &pool,
                session4,
                run4,
                Some(repo_root),
                action2,
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();
        assert_eq!(state_of(&pool, fourth).await, "pending");
    }

    #[tokio::test]
    async fn pattern_resolve_of_unlearnable_action_fails() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let (session, run) = seed_session_run(&pool).await;
        let broker = ApprovalBroker::new();

        let unlearnable_action = ProposedAction::ExecuteCommand {
            program: "python".to_string(),
            args: vec!["script.py".to_string()],
            environment: Vec::new(),
            cwd: None,
        };

        let first = broker
            .request(
                &pool,
                session,
                run,
                Some("/home/user/repo"),
                unlearnable_action,
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();

        let err = broker
            .resolve(
                &pool,
                first,
                ApprovalDecision::Approve,
                ApprovalScope::Pattern,
                "tester".to_string(),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, ApprovalError::PatternUnavailable));
        assert_eq!(state_of(&pool, first).await, "pending");
    }

    #[tokio::test]
    async fn pattern_column_is_stamped_at_resolve_time() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let (session, run) = seed_session_run(&pool).await;
        let broker = ApprovalBroker::new();

        let action = ProposedAction::ExecuteCommand {
            program: "npm".to_string(),
            args: vec!["run".to_string(), "build".to_string()],
            environment: Vec::new(),
            cwd: None,
        };

        let first = broker
            .request(
                &pool,
                session,
                run,
                Some("/home/user/repo"),
                action,
                sample_risk(),
                vec![],
                None,
            )
            .await
            .unwrap();

        broker
            .resolve(
                &pool,
                first,
                ApprovalDecision::Approve,
                ApprovalScope::Pattern,
                "tester".to_string(),
            )
            .await
            .unwrap();

        let (pattern,): (Option<String>,) =
            sqlx::query_as("SELECT pattern FROM approvals WHERE id = ?")
                .bind(first.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pattern.as_deref(), Some("npm run build *"));
    }
}
