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
//! client) can subscribe independently.

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
    /// A stored row could not be decoded (should never happen; the daemon wrote
    /// it).
    #[error("corrupt approval row: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
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
        action: ProposedAction,
        risk: Risk,
        capabilities: Vec<Capability>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<ApprovalId, ApprovalError> {
        let approval_id = ApprovalId::new();
        let digest = action_digest(&action)?;
        let action_json = serde_json::to_string(&action)?;
        let risk_json = serde_json::to_string(&risk)?;
        let capabilities_json = serde_json::to_string(&capabilities)?;
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let expires_str = expires_at.map(|t| t.to_rfc3339());

        let auto_approve = self.run_scoped_match(pool, run_id, &digest).await?;

        let mut tx = pool.begin().await?;
        let state = if auto_approve {
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
        .bind(if auto_approve {
            Some("auto:run-scope")
        } else {
            None
        })
        .bind(&now_str)
        .bind(if auto_approve { Some(&now_str) } else { None })
        .bind(&expires_str)
        .execute(&mut *tx)
        .await?;

        // ApprovalRequested is always recorded (RULE: persist before publish).
        let requested_seq = next_sequence(&mut *tx, session_id).await?;
        let requested = EventBody::ApprovalRequested {
            approval_id,
            action,
            risk,
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
        let resolved = if auto_approve {
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
            None
        };

        tx.commit().await?;

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

        // Register the waiter: pre-resolved for auto-approval so `await_decision`
        // returns `Approve` without a human step; empty otherwise (parked).
        let initial = auto_approve.then_some(ApprovalDecision::Approve);
        self.register_waiter(approval_id, initial).await;
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
            let guard = self.waiters.lock().expect("waiters mutex poisoned");
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
                self.waiters
                    .lock()
                    .expect("waiters mutex poisoned")
                    .remove(&approval_id);
                return Ok(decision);
            }
            if rx.changed().await.is_err() {
                self.waiters
                    .lock()
                    .expect("waiters mutex poisoned")
                    .remove(&approval_id);
                return Err(ApprovalError::WaiterGone { approval_id });
            }
        }
    }

    /// Resolve a `pending` approval: update the row (`approved`/`rejected`,
    /// `resolved_by`, `resolved_at`, and the real `scope`), append
    /// `ApprovalResolved`, then wake the parked waiter. `Run` scope leaves an
    /// approved row that [`request`](Self::request) treats as an auto-approval
    /// pattern for identical later proposals; `Once` does not.
    pub async fn resolve(
        &self,
        pool: &SqlitePool,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
        scope: ApprovalScope,
        resolved_by: String,
    ) -> Result<(), ApprovalError> {
        let state = decision_state(decision)?;
        let scope_db = scope_to_db(scope)?;

        let existing: Option<(String, String)> = sqlx::query_as(
            "SELECT a.state, r.session_id FROM approvals a \
             JOIN runs r ON a.run_id = r.id WHERE a.id = ?",
        )
        .bind(approval_id.to_string())
        .fetch_optional(pool)
        .await?;
        let (current_state, session_id) =
            existing.ok_or(ApprovalError::NotFound { approval_id })?;
        if current_state != "pending" {
            return Err(ApprovalError::AlreadyResolved {
                approval_id,
                state: current_state,
            });
        }
        let session_id = parse_session_id(&session_id)?;
        let now = Utc::now();
        let now_str = now.to_rfc3339();

        let mut tx = pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE approvals SET state = ?, scope = ?, resolved_by = ?, resolved_at = ? \
             WHERE id = ? AND state = 'pending'",
        )
        .bind(state.as_db())
        .bind(scope_db)
        .bind(&resolved_by)
        .bind(&now_str)
        .bind(approval_id.to_string())
        .execute(&mut *tx)
        .await?;
        // Lost the race to another resolver / an expiry between our read and here.
        if updated.rows_affected() != 1 {
            return Err(ApprovalError::AlreadyResolved {
                approval_id,
                state: "resolved".to_string(),
            });
        }

        let seq = next_sequence(&mut *tx, session_id).await?;
        append_event(
            &mut *tx,
            session_id,
            seq,
            &Actor::Human {
                user_id: UserId(resolved_by),
            },
            &EventBody::ApprovalResolved {
                approval_id,
                decision,
            },
            &now_str,
        )
        .await?;
        tx.commit().await?;

        self.wake(approval_id, decision).await;
        Ok(())
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

            let mut tx = pool.begin().await?;
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
            tx.commit().await?;

            self.wake(approval_id, ApprovalDecision::Reject).await;
            expired += 1;
        }
        Ok(expired)
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
            let mut guard = self.waiters.lock().expect("waiters mutex poisoned");
            guard
                .entry(approval.approval_id)
                .or_insert_with(|| watch::channel(None).0);
            drop(guard);
            pending.push(approval);
        }
        Ok(pending)
    }

    /// Insert (or replace) a waiter for `approval_id`, optionally pre-loaded with
    /// a decision (auto-approval).
    async fn register_waiter(&self, approval_id: ApprovalId, initial: Option<ApprovalDecision>) {
        let (sender, _rx) = watch::channel(initial);
        self.waiters
            .lock()
            .expect("waiters mutex poisoned")
            .insert(approval_id, sender);
    }

    /// Deliver `decision` to a parked waiter, if any. `send_replace` never fails
    /// even when nobody is subscribed yet — the value is retained for a later
    /// subscriber.
    async fn wake(&self, approval_id: ApprovalId, decision: ApprovalDecision) {
        let guard = self.waiters.lock().expect("waiters mutex poisoned");
        if let Some(sender) = guard.get(&approval_id) {
            sender.send_replace(Some(decision));
        }
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

    /// Create a session (via the ledger helper) and a minimal `runs` row so the
    /// `approvals.run_id` foreign key resolves; return both ids.
    async fn seed_session_run(pool: &SqlitePool) -> (SessionId, RunId) {
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

        (session_id, run_id)
    }

    fn sample_action() -> ProposedAction {
        ProposedAction::ExecuteCommand {
            program: "cargo".to_string(),
            args: vec!["test".to_string()],
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
                ProposedAction::ExecuteCommand {
                    program: "cargo".to_string(),
                    args: vec!["build".to_string()],
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
}
