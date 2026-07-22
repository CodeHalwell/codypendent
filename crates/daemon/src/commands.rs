//! Command handling and the crash-consistent write path (STEP 1.3).
//!
//! This is the single most important algorithm in the product: the *idempotent*,
//! *crash-consistent* application of a client [`Command`]. Every command follows
//! the same six-step sequence (Chapter 03 "Crash consistency"):
//!
//! 1. **Idempotency check first.** Look up `commands.idempotency_key`. A row in
//!    `status = 'applied'` returns its recorded `result_json` verbatim — nothing
//!    re-executes (this is the exit criterion: *duplicate delivery produces one
//!    effect and one result*). A row in `status = 'received'` means a crash
//!    landed mid-apply, so we *resume reconciliation* rather than re-execute.
//! 2. **Validate.** Schema ([`CommandBody::Unknown`] → `protocol.unsupported-payload`),
//!    session/run existence where required, and the caller's [`ClientRole`]
//!    ([`ClientRole::Observer`] issuing `StartRun` → `protocol.role-denied`).
//!    Handlers return a structured [`CodypendentError`]; they never panic.
//! 3. **One transaction.** Insert the `commands` row (`received`), insert any
//!    `pending_effects`, append the resulting ledger event(s) — allocating
//!    `sequence` *inside this tx* (the [`crate::approvals`] atomic-append
//!    pattern) — update the projection rows (`runs`), set `commands.status =
//!    'applied'` with its `result_json`, and COMMIT.
//! 4. **Perform the external side effect** (if any) *outside* the transaction.
//!    Almost every Phase 1 command has none — the real tool effects happen in
//!    the agent loop (STEP 1.10). `ResolveApproval`'s effect (flip the approval
//!    row + append `ApprovalResolved`) is folded *into* the command transaction
//!    via [`crate::approvals::ApprovalBroker::resolve_in_tx`], so its
//!    `expected_revision` guard, the append, and the revision bump are all
//!    atomic (issue #6 item 2); only the parked-waiter wake happens after commit.
//! 5. **Persist the outcome** (`pending_effects` → `performed`/`reconciled`,
//!    append an outcome event) once the effect completes.
//! 6. **Publish** the persisted events through the [`SubscriptionHub`] — *after*
//!    commit, never before (persist before publish, RULE 2).
//!
//! Because steps 3's `received`→`applied` transition is atomic, a committed
//! `commands` row is always `applied`; the `received` state is only durable for
//! rows written by a crash-injection test. Startup recovery
//! ([`CommandProcessor::reconcile_pending_effects`]) sweeps any orphaned
//! `pending_effects`; STEP 1.14 extends that recovery.

use std::str::FromStr;

use chrono::Utc;
use codypendent_protocol::{
    Actor, AgentMode, ApprovalDecision, ApprovalScope, ClientId, ClientRole, CodypendentError,
    Command, CommandBody, CommandId, EventBody, RunId, RunState, SessionEvent, SessionId,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::approvals::{ApprovalBroker, ApprovalError};
use crate::projections;
use crate::subscriptions::SubscriptionHub;

/// A run's resolved model policy is not carried by the Phase 1 `StartRun`
/// command; the write path records this default (a `models.toml` profile id).
const DEFAULT_MODEL_POLICY: &str = "hosted-default";
/// Likewise the run budget: an empty JSON object until the agent loop sets one.
const DEFAULT_BUDGET_JSON: &str = "{}";

/// Who is issuing a command, for validation and event attribution. The role
/// gates *which* commands are permitted (see [`role_permits`]); the client id is
/// recorded on the `commands` row and stamped on the events it causes.
#[derive(Debug, Clone)]
pub struct ApplyContext {
    pub client_id: ClientId,
    pub role: ClientRole,
}

/// The recorded result of applying a command, stored as `commands.result_json`
/// and replayed **verbatim** on an idempotent repeat. Two applications of the
/// same envelope therefore return an equal `CommandOutcome`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandOutcome {
    pub command_id: CommandId,
    /// The session created by a `CreateSession`, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_session: Option<SessionId>,
    /// The run created by a `StartRun`, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_run: Option<RunId>,
    /// The sequence of the last event this command appended, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sequence: Option<u64>,
    /// Whether THIS call freshly applied the command, as opposed to replaying a
    /// recorded outcome for a duplicate idempotency key. Never persisted, so a
    /// replayed outcome (deserialized from the `commands` row) is always `false`
    /// — which is exactly how the server launches the executor **once** per run
    /// instead of again on every duplicate `StartRun` delivery.
    #[serde(skip)]
    pub newly_applied: bool,
}

/// Applies commands through the crash-consistent write path, owning the shared
/// [`SubscriptionHub`] it publishes to and the [`ApprovalBroker`] it delegates
/// approval resolutions to. Cloning shares both (each is `Arc`-backed).
#[derive(Debug, Clone, Default)]
pub struct CommandProcessor {
    subscriptions: SubscriptionHub,
    approvals: ApprovalBroker,
}

impl CommandProcessor {
    /// A processor wired to a shared subscription hub and approval broker.
    pub fn new(subscriptions: SubscriptionHub, approvals: ApprovalBroker) -> Self {
        Self {
            subscriptions,
            approvals,
        }
    }

    /// The shared fan-out this processor publishes committed events to. Callers
    /// (the protocol server, tests) clone it to `subscribe`.
    pub fn subscriptions(&self) -> &SubscriptionHub {
        &self.subscriptions
    }

    /// The approval broker this processor delegates `ResolveApproval` to.
    pub fn approvals(&self) -> &ApprovalBroker {
        &self.approvals
    }

    /// Apply one command through the full six-step sequence. Idempotent on
    /// `idempotency_key`; returns a structured [`CodypendentError`] on any bad
    /// input, never panics.
    pub async fn apply(
        &self,
        pool: &SqlitePool,
        ctx: ApplyContext,
        command: Command,
    ) -> Result<CommandOutcome, CodypendentError> {
        // Step 1: idempotency check FIRST.
        if let Some(existing) = lookup_command(pool, &command.idempotency_key)
            .await
            .map_err(internal_error)?
        {
            return self.handle_existing(pool, existing).await;
        }

        // Step 2: validate (schema, existence, role).
        self.validate(pool, &ctx, &command).await?;

        // Steps 3-6 per variant.
        match command.body.clone() {
            CommandBody::CreateSession { title, .. } => {
                self.apply_create_session(pool, &ctx, &command, title).await
            }
            CommandBody::StartRun {
                session_id,
                objective,
                mode,
                // `repository` is consumed by the server when it builds the
                // executor's `RunLaunch` (it decides the run's repository
                // identity), not by the write path — the ledger row is the same.
                ..
            } => {
                self.apply_start_run(pool, &ctx, &command, session_id, objective, mode)
                    .await
            }
            CommandBody::SubmitUserInput {
                session_id, text, ..
            } => {
                self.apply_submit_input(pool, &ctx, &command, session_id, text)
                    .await
            }
            CommandBody::QueueSteering { run_id, .. } => {
                self.apply_queue_steering(pool, &ctx, &command, run_id)
                    .await
            }
            CommandBody::CancelRun { run_id } => {
                self.apply_run_state(pool, &ctx, &command, run_id, RunState::Cancelled)
                    .await
            }
            CommandBody::PauseRun { run_id } => {
                self.apply_run_state(pool, &ctx, &command, run_id, RunState::Paused)
                    .await
            }
            CommandBody::ResumeRun { run_id } => {
                self.apply_run_state(pool, &ctx, &command, run_id, RunState::Running)
                    .await
            }
            CommandBody::ResolveApproval {
                approval_id,
                decision,
                scope,
            } => {
                self.apply_resolve_approval(pool, &ctx, &command, approval_id, decision, scope)
                    .await
            }
            // `AttachSession`/`Unknown` are already rejected in `validate`; this
            // catch-all keeps the (non_exhaustive) match total and restates the
            // rejection defensively.
            _ => Err(rejected_for_body(&command.body)),
        }
    }

    /// Scan `pending_effects` still in flight (`intended`, or `performed`
    /// awaiting an outcome) and reconcile them against reality, then mark each
    /// `reconciled`/`abandoned` and append a reconciliation event. Returns how
    /// many effects were reconciled. Called on startup and by the `received`
    /// resume path; STEP 1.14 layers richer recovery on top.
    pub async fn reconcile_pending_effects(&self, pool: &SqlitePool) -> anyhow::Result<usize> {
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT id, command_id, kind, state FROM pending_effects \
             WHERE state IN ('intended', 'performed')",
        )
        .fetch_all(pool)
        .await?;

        let mut reconciled = 0usize;
        for (id, command_id, kind, state) in rows {
            if self
                .reconcile_effect(pool, &id, &command_id, &kind, &state)
                .await?
            {
                reconciled += 1;
            }
        }
        Ok(reconciled)
    }

    // --- idempotency branches -------------------------------------------------

    /// Handle a command whose `idempotency_key` is already recorded.
    async fn handle_existing(
        &self,
        pool: &SqlitePool,
        existing: ExistingCommand,
    ) -> Result<CommandOutcome, CodypendentError> {
        match existing.status.as_str() {
            // Applied: replay the recorded outcome verbatim, execute nothing.
            "applied" => {
                let json = existing
                    .result_json
                    .ok_or_else(|| internal_error("applied command row is missing result_json"))?;
                serde_json::from_str(&json).map_err(internal_error)
            }
            // Received: a crash landed mid-apply — resume, do not re-execute.
            "received" => self.resume_received(pool, existing).await,
            other => Err(internal_error(format!(
                "command in unexpected status {other:?}"
            ))),
        }
    }

    /// Resume a command that committed its `received` row but crashed before it
    /// finished. Reconcile its pending effects, drive its external effect to
    /// completion idempotently (only `ResolveApproval` has one in Phase 1), then
    /// mark it `applied`.
    async fn resume_received(
        &self,
        pool: &SqlitePool,
        existing: ExistingCommand,
    ) -> Result<CommandOutcome, CodypendentError> {
        self.reconcile_command_effects(pool, &existing.command_id)
            .await
            .map_err(internal_error)?;

        let body: CommandBody = serde_json::from_str(&existing.body).map_err(internal_error)?;
        if let CommandBody::ResolveApproval {
            approval_id,
            decision,
            scope,
        } = body
        {
            match self
                .approvals
                .resolve(
                    pool,
                    approval_id,
                    decision,
                    scope,
                    existing.client_id.clone(),
                )
                .await
            {
                // Completed now: publish the exact appended `ApprovalResolved`
                // so live subscribers observe it instead of a sequence gap they
                // only close on re-attach (persist-before-publish: `resolve`
                // committed before returning the event).
                Ok(event) => {
                    if let Some(session_id) = existing.session_id {
                        self.subscriptions.publish(session_id, event);
                    }
                }
                // Already resolved before the crash — the effect is done exactly
                // once and its event was published by whoever resolved it.
                Err(ApprovalError::AlreadyResolved { .. }) => {}
                Err(e) => return Err(map_approval_error(e)),
            }
        }

        let last_sequence = match existing.session_id {
            Some(session_id) => max_sequence(pool, session_id)
                .await
                .map_err(internal_error)?,
            None => None,
        };
        let outcome = CommandOutcome {
            command_id: existing.command_id,
            created_session: None,
            created_run: None,
            last_sequence,
            newly_applied: false,
        };
        finalize_applied(pool, existing.command_id, &outcome)
            .await
            .map_err(internal_error)?;
        Ok(outcome)
    }

    // --- validation -----------------------------------------------------------

    async fn validate(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
    ) -> Result<(), CodypendentError> {
        // Schema: a body from a newer client, or attach (a connection-level
        // concern, not the generic write path — STEP 1.11).
        match &command.body {
            CommandBody::Unknown => {
                return Err(CodypendentError::new(
                    "protocol.unsupported-payload",
                    "unknown command body",
                    false,
                ));
            }
            CommandBody::AttachSession { .. } => {
                return Err(CodypendentError::new(
                    "protocol.attach-is-connection-level",
                    "AttachSession is handled by the connection layer, not the command write path",
                    false,
                ));
            }
            _ => {}
        }

        // Role: checked before existence so a denied role never leaks whether a
        // resource exists, and `Observer`-issues-`StartRun` is `role-denied`
        // regardless of the session.
        if !role_permits(ctx.role, &command.body) {
            return Err(CodypendentError::new(
                "protocol.role-denied",
                format!("role {:?} may not issue this command", ctx.role),
                false,
            ));
        }

        // Existence where the command targets pre-existing state.
        match &command.body {
            CommandBody::StartRun { session_id, .. }
            | CommandBody::SubmitUserInput { session_id, .. } => {
                if !session_exists(pool, *session_id)
                    .await
                    .map_err(internal_error)?
                {
                    return Err(CodypendentError::new(
                        "protocol.session-not-found",
                        format!("no session {session_id}"),
                        false,
                    ));
                }
            }
            CommandBody::CancelRun { run_id }
            | CommandBody::PauseRun { run_id }
            | CommandBody::ResumeRun { run_id } => {
                let state = projections::load_run_state(pool, *run_id)
                    .await
                    .map_err(internal_error)?
                    .ok_or_else(|| run_not_found(*run_id))?;
                validate_run_transition(&command.body, *run_id, state)?;
            }
            CommandBody::QueueSteering { run_id, .. } => {
                if projections::run_session(pool, *run_id)
                    .await
                    .map_err(internal_error)?
                    .is_none()
                {
                    return Err(run_not_found(*run_id));
                }
            }
            CommandBody::ResolveApproval { approval_id, .. } => {
                let existing_session = approval_session(pool, *approval_id)
                    .await
                    .map_err(internal_error)?;
                if existing_session.is_none() {
                    return Err(CodypendentError::new(
                        "approval.not-found",
                        format!("no approval {approval_id}"),
                        false,
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }

    // --- per-command handlers -------------------------------------------------

    async fn apply_create_session(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        title: String,
    ) -> Result<CommandOutcome, CodypendentError> {
        let session_id = SessionId::new();
        // The session row is created *inside* the write transaction (inlined
        // rather than `ledger::create_session`, which takes a pool) so it is
        // atomic with the `SessionCreated` event, the `commands` row, and the
        // idempotency guarantee — a retry with the same key can never mint a
        // second session.
        let events = vec![(
            Actor::Client {
                client_id: ctx.client_id,
            },
            EventBody::SessionCreated {
                title: title.clone(),
            },
        )];
        self.run_transaction(
            pool,
            ctx,
            command,
            Some(session_id),
            session_id,
            PreInsert::Session {
                session_id,
                title: &title,
            },
            events,
            ProjectionOp::None,
            (Some(session_id), None),
            // The session is being created now, at revision 0. There is no prior
            // session to guard, so `expected_revision` is ignored here (the
            // sensible rule for `CreateSession`).
            RevisionOp::Establish,
        )
        .await
    }

    async fn apply_start_run(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        session_id: SessionId,
        objective: String,
        mode: AgentMode,
    ) -> Result<CommandOutcome, CodypendentError> {
        let run_id = RunId::new();
        let events = vec![(
            Actor::Client {
                client_id: ctx.client_id,
            },
            EventBody::RunStarted {
                run_id,
                objective: objective.clone(),
                mode,
            },
        )];
        self.run_transaction(
            pool,
            ctx,
            command,
            Some(session_id),
            session_id,
            PreInsert::None,
            events,
            ProjectionOp::InsertRun {
                run_id,
                session_id,
                objective,
                mode,
            },
            (None, Some(run_id)),
            RevisionOp::Bump {
                expected: command.expected_revision,
            },
        )
        .await
    }

    async fn apply_submit_input(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        session_id: SessionId,
        text: String,
    ) -> Result<CommandOutcome, CodypendentError> {
        // Phase 1 minimal: record the input as a note; the agent loop consumes
        // input/steering more richly later (STEP 1.10).
        let events = vec![(
            Actor::Client {
                client_id: ctx.client_id,
            },
            // Session-level user input — not tied to one run's transcript.
            EventBody::NoteAppended { text, run_id: None },
        )];
        self.run_transaction(
            pool,
            ctx,
            command,
            Some(session_id),
            session_id,
            PreInsert::None,
            events,
            ProjectionOp::None,
            (None, None),
            RevisionOp::Bump {
                expected: command.expected_revision,
            },
        )
        .await
    }

    async fn apply_queue_steering(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        run_id: RunId,
    ) -> Result<CommandOutcome, CodypendentError> {
        let session_id = projections::run_session(pool, run_id)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| run_not_found(run_id))?;
        let events = vec![(
            Actor::Client {
                client_id: ctx.client_id,
            },
            EventBody::SteeringQueued { run_id },
        )];
        self.run_transaction(
            pool,
            ctx,
            command,
            Some(session_id),
            session_id,
            PreInsert::None,
            events,
            ProjectionOp::None,
            (None, None),
            RevisionOp::Bump {
                expected: command.expected_revision,
            },
        )
        .await
    }

    async fn apply_run_state(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        run_id: RunId,
        state: RunState,
    ) -> Result<CommandOutcome, CodypendentError> {
        let session_id = projections::run_session(pool, run_id)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| run_not_found(run_id))?;
        let events = vec![(
            Actor::Client {
                client_id: ctx.client_id,
            },
            EventBody::RunStateChanged { run_id, state },
        )];
        self.run_transaction(
            pool,
            ctx,
            command,
            Some(session_id),
            session_id,
            PreInsert::None,
            events,
            ProjectionOp::SetRunState { run_id, state },
            (None, None),
            RevisionOp::Bump {
                expected: command.expected_revision,
            },
        )
        .await
    }

    /// `ResolveApproval` is the one Phase 1 command with an external effect (flip
    /// the approval row + append `ApprovalResolved` + wake the parked runtime
    /// waiter). ONE transaction holds the whole command: the `received` command
    /// row, the `expected_revision` guard, the broker's flip + append (via
    /// [`ApprovalBroker::resolve_in_tx`]), the session-revision bump, and the flip
    /// to `applied`. Holding the guard *and* the bump in the same transaction as
    /// the append is what makes two commands sharing one `expected_revision`
    /// mutually exclusive (issue #6 item 2b, previously three separate txs). After
    /// commit we publish *exactly* the appended event (never the session tail,
    /// which a concurrent append may have changed — item 2a) and wake the waiter.
    async fn apply_resolve_approval(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        approval_id: codypendent_protocol::ApprovalId,
        decision: ApprovalDecision,
        scope: ApprovalScope,
    ) -> Result<CommandOutcome, CodypendentError> {
        let session_id = approval_session(pool, approval_id)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| approval_not_found(approval_id))?;

        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let body_json = serde_json::to_string(&command.body).map_err(internal_error)?;

        let mut tx = pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(internal_error)?;

        // 1. Command row (received). A concurrent duplicate that loses this insert
        //    replays the recorded outcome instead of erroring.
        if let Err(err) = sqlx::query(
            "INSERT INTO commands \
             (id, idempotency_key, session_id, client_id, body, status, received_at) \
             VALUES (?, ?, ?, ?, ?, 'received', ?)",
        )
        .bind(command.command_id.to_string())
        .bind(&command.idempotency_key)
        .bind(session_id.to_string())
        .bind(ctx.client_id.to_string())
        .bind(&body_json)
        .bind(&now_str)
        .execute(&mut *tx)
        .await
        {
            let _ = tx.rollback().await;
            let err = anyhow::Error::from(err);
            if is_unique_violation(&err) {
                if let Some(existing) = lookup_command(pool, &command.idempotency_key)
                    .await
                    .map_err(internal_error)?
                {
                    return self.handle_existing(pool, existing).await;
                }
            }
            return Err(internal_error(err));
        }

        // 2. Optimistic-concurrency guard, read under the write lock so no
        //    concurrent ResolveApproval can slip between this check and the bump.
        if let Some(expected) = command.expected_revision {
            let (current,): (i64,) = sqlx::query_as("SELECT revision FROM sessions WHERE id = ?")
                .bind(session_id.to_string())
                .fetch_one(&mut *tx)
                .await
                .map_err(internal_error)?;
            let current = u64::try_from(current).map_err(internal_error)?;
            if expected != current {
                let _ = tx.rollback().await;
                return Err(revision_conflict(expected, current));
            }
        }

        // 3. The external effect, INSIDE this tx: flip the approval and append
        //    `ApprovalResolved`, getting back that exact event to publish.
        let event = match self
            .approvals
            .resolve_in_tx(
                &mut tx,
                approval_id,
                decision,
                scope,
                ctx.client_id.to_string(),
                now,
            )
            .await
        {
            Ok(event) => {
                // 4. Bump the session revision, atomic with the append it reflects.
                sqlx::query(
                    "UPDATE sessions SET revision = revision + 1, updated_at = ? WHERE id = ?",
                )
                .bind(&now_str)
                .bind(session_id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(internal_error)?;
                Some(event)
            }
            // Already resolved (a prior delivery, another resolver, or an expiry):
            // a successful no-op — the decision is already on the ledger. Record
            // the command `applied` with no new event and no bump, matching the
            // resume-replay path so first delivery and replay agree.
            Err(ApprovalError::AlreadyResolved { .. }) => None,
            Err(err @ ApprovalError::NotFound { .. }) => {
                let _ = tx.rollback().await;
                return Err(map_approval_error(err));
            }
            Err(err) => {
                let _ = tx.rollback().await;
                return Err(map_approval_error(err));
            }
        };

        // 5. Compute the outcome and flip the command to `applied`, still in the tx.
        let last_sequence = match &event {
            Some(event) => Some(event.sequence),
            None => tx_max_sequence(&mut *tx, session_id)
                .await
                .map_err(internal_error)?,
        };
        let outcome = CommandOutcome {
            command_id: command.command_id,
            created_session: None,
            created_run: None,
            last_sequence,
            newly_applied: false,
        };
        sqlx::query(
            "UPDATE commands SET status = 'applied', result_json = ?, applied_at = ? WHERE id = ?",
        )
        .bind(serde_json::to_string(&outcome).map_err(internal_error)?)
        .bind(&now_str)
        .bind(command.command_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;

        tx.commit().await.map_err(internal_error)?;

        // 6. Post-commit (persist before publish): wake the parked runtime waiter
        //    and publish exactly the appended event.
        if let Some(event) = event {
            self.approvals.wake(approval_id, decision).await;
            self.subscriptions.publish(session_id, event);
        }

        Ok(outcome)
    }

    // --- the transaction ------------------------------------------------------

    /// Run steps 3 and 6 for an effect-free command: one transaction that
    /// records the command, appends its events (allocating sequence inside the
    /// tx), updates projections, and commits `applied`; then publishes the
    /// committed events. Infrastructure failures become an `internal` error.
    #[allow(clippy::too_many_arguments)] // the write path threads many typed pieces through one atomic tx.
    async fn run_transaction(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        command_session: Option<SessionId>,
        event_session: SessionId,
        pre: PreInsert<'_>,
        events: Vec<(Actor, EventBody)>,
        projection: ProjectionOp,
        created: (Option<SessionId>, Option<RunId>),
        revision: RevisionOp,
    ) -> Result<CommandOutcome, CodypendentError> {
        let committed = self
            .commit(
                pool,
                ctx,
                command,
                command_session,
                event_session,
                pre,
                events,
                projection,
                created,
                revision,
            )
            .await;

        let (outcome, persisted) = match committed {
            Ok(value) => value,
            Err(err) => {
                // A failed `expected_revision` guard is a structured protocol
                // conflict, not an infrastructure failure — the tx rolled back,
                // so nothing was applied.
                if let Some(conflict) = err.downcast_ref::<RevisionConflict>() {
                    return Err(revision_conflict(conflict.expected, conflict.actual));
                }
                // A run-state transition rejected by the atomic conditional
                // write (FP-3) — likewise a structured protocol rejection, not
                // an infrastructure failure; the tx rolled back.
                if let Some(rejected) = err.downcast_ref::<RunTransitionRejected>() {
                    return Err(rejected.0.clone());
                }
                // A concurrent duplicate delivery won the race to insert the
                // `commands` row (its `UNIQUE(idempotency_key)`/PK tripped). That
                // is not `internal.command-apply-failed`: the winner already
                // recorded the outcome, so replay it via the existing-command
                // path (RULE: duplicate delivery = one effect, one result). We
                // re-run the idempotency lookup and only replay when a row with
                // this key exists, so an unrelated unique violation still errors.
                if is_unique_violation(&err) {
                    if let Some(existing) = lookup_command(pool, &command.idempotency_key)
                        .await
                        .map_err(internal_error)?
                    {
                        return self.handle_existing(pool, existing).await;
                    }
                }
                return Err(internal_error(err));
            }
        };

        // Step 6: publish only after the commit (persist before publish).
        for event in persisted {
            self.subscriptions.publish(event_session, event);
        }
        Ok(outcome)
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        command_session: Option<SessionId>,
        event_session: SessionId,
        pre: PreInsert<'_>,
        events: Vec<(Actor, EventBody)>,
        projection: ProjectionOp,
        created: (Option<SessionId>, Option<RunId>),
        revision: RevisionOp,
    ) -> anyhow::Result<(CommandOutcome, Vec<SessionEvent>)> {
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

        // Optimistic-concurrency guard + revision advance, atomic (inside this
        // tx) with the append it protects. `Establish` (CreateSession) inserts a
        // fresh session at revision 0 below and ignores `expected_revision`;
        // `Bump` checks the guard against the *live* revision — read under the
        // write lock so no concurrent command can slip between check and bump —
        // and advances it. On a mismatch we abort the whole tx (nothing applied).
        if let RevisionOp::Bump { expected } = revision {
            let (current,): (i64,) = sqlx::query_as("SELECT revision FROM sessions WHERE id = ?")
                .bind(event_session.to_string())
                .fetch_one(&mut *tx)
                .await?;
            let current = u64::try_from(current)?;
            if let Some(expected) = expected {
                if expected != current {
                    return Err(RevisionConflict {
                        expected,
                        actual: current,
                    }
                    .into());
                }
            }
            sqlx::query("UPDATE sessions SET revision = revision + 1, updated_at = ? WHERE id = ?")
                .bind(&now_str)
                .bind(event_session.to_string())
                .execute(&mut *tx)
                .await?;
        }

        // commands row (received).
        sqlx::query(
            "INSERT INTO commands \
             (id, idempotency_key, session_id, client_id, body, status, received_at) \
             VALUES (?, ?, ?, ?, ?, 'received', ?)",
        )
        .bind(command.command_id.to_string())
        .bind(&command.idempotency_key)
        .bind(command_session.map(|s| s.to_string()))
        .bind(ctx.client_id.to_string())
        .bind(serde_json::to_string(&command.body)?)
        .bind(&now_str)
        .execute(&mut *tx)
        .await?;

        // Session pre-insert must precede its events (the events FK references
        // sessions(id)).
        if let PreInsert::Session { session_id, title } = pre {
            sqlx::query(
                "INSERT INTO sessions (id, title, state, created_at, updated_at, revision) \
                 VALUES (?, ?, 'open', ?, ?, 0)",
            )
            .bind(session_id.to_string())
            .bind(title)
            .bind(&now_str)
            .bind(&now_str)
            .execute(&mut *tx)
            .await?;
        }

        // Append events, allocating each sequence inside this tx.
        let mut persisted = Vec::with_capacity(events.len());
        for (actor, body) in events {
            let sequence = next_sequence(&mut *tx, event_session).await?;
            append_event(
                &mut *tx,
                event_session,
                sequence,
                &actor,
                &body,
                &now_str,
                Some(command.command_id),
            )
            .await?;
            persisted.push(SessionEvent {
                sequence: u64::try_from(sequence)?,
                occurred_at: now,
                causation_id: Some(command.command_id),
                correlation_id: None,
                actor,
                body,
            });
        }

        // Projection rows.
        match projection {
            ProjectionOp::None => {}
            ProjectionOp::InsertRun {
                run_id,
                session_id,
                objective,
                mode,
            } => {
                projections::insert_run(
                    &mut *tx,
                    run_id,
                    session_id,
                    &objective,
                    mode,
                    DEFAULT_MODEL_POLICY,
                    DEFAULT_BUDGET_JSON,
                )
                .await?;
            }
            ProjectionOp::SetRunState { run_id, state } => {
                // Assert the CURRENT state is legal for this transition via a
                // single conditional UPDATE, not a separate read-then-write
                // (FP-3): `validate()`'s pre-transaction read can go stale
                // between two concurrent lifecycle commands on the same run —
                // e.g. a `CancelRun` and a `PauseRun` both reading `Running`
                // and both passing that check — so the write itself
                // re-asserts the prior state and only applies when it still
                // holds. `BEGIN IMMEDIATE` above means whichever of two
                // racing commands reaches this point SECOND sees the FIRST
                // one's already-committed state, so an invalid transition
                // (e.g. a `Cancelled` run flipped back to `Paused`) can never
                // commit.
                let legal_from = legal_prior_states(&command.body, run_id);
                let affected =
                    projections::set_run_state_if_legal(&mut *tx, run_id, &legal_from, state)
                        .await?;
                if affected == 0 {
                    // Not legal from the run's CURRENT state (re-read fresh,
                    // under this transaction's write lock) — either
                    // `validate()`'s earlier read was stale (a concurrent
                    // command committed first) or the run no longer exists.
                    // Reject with the same structured error `validate()` would
                    // have produced; the whole transaction rolls back (nothing
                    // applied).
                    let current = projections::load_run_state(&mut *tx, run_id).await?;
                    let rejection = match current {
                        Some(fresh_state) => validate_run_transition(
                            &command.body,
                            run_id,
                            fresh_state,
                        )
                        .expect_err(
                            "a state excluded from legal_prior_states must fail validate_run_transition",
                        ),
                        None => run_not_found(run_id),
                    };
                    return Err(RunTransitionRejected(rejection).into());
                }
            }
        }

        let outcome = CommandOutcome {
            command_id: command.command_id,
            created_session: created.0,
            created_run: created.1,
            last_sequence: persisted.last().map(|e| e.sequence),
            // `run_transaction` runs only on the FIRST application (the
            // idempotency check returns a replay before reaching here), so this
            // is the one place `newly_applied` is true — the signal the server
            // uses to launch the executor exactly once per created run.
            newly_applied: true,
        };

        // Flip received -> applied with the recorded outcome, still in the tx.
        sqlx::query(
            "UPDATE commands SET status = 'applied', result_json = ?, applied_at = ? WHERE id = ?",
        )
        .bind(serde_json::to_string(&outcome)?)
        .bind(&now_str)
        .bind(command.command_id.to_string())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok((outcome, persisted))
    }

    // --- pending-effect reconciliation ---------------------------------------

    async fn reconcile_command_effects(
        &self,
        pool: &SqlitePool,
        command_id: &CommandId,
    ) -> anyhow::Result<usize> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT id, kind, state FROM pending_effects \
             WHERE command_id = ? AND state IN ('intended', 'performed')",
        )
        .bind(command_id.to_string())
        .fetch_all(pool)
        .await?;

        let command_id = command_id.to_string();
        let mut reconciled = 0usize;
        for (id, kind, state) in rows {
            if self
                .reconcile_effect(pool, &id, &command_id, &kind, &state)
                .await?
            {
                reconciled += 1;
            }
        }
        Ok(reconciled)
    }

    /// Reconcile one pending effect. Phase 1.3 has no verifiable real-world
    /// effects yet (tool effects land in the agent loop, STEP 1.10), so an
    /// `intended` effect that never ran is **abandoned** — re-performing it blind
    /// would risk the very duplicate the crash-consistency contract forbids —
    /// and a `performed` effect awaiting its outcome is **reconciled**. A
    /// reconciliation `NoteAppended` records the decision on the session ledger.
    /// STEP 1.14 replaces the heuristic with real reality-checks. Returns whether
    /// this call changed the row (false if another sweep won the race).
    async fn reconcile_effect(
        &self,
        pool: &SqlitePool,
        id: &str,
        command_id: &str,
        kind: &str,
        state: &str,
    ) -> anyhow::Result<bool> {
        let new_state = if state == "performed" {
            "reconciled"
        } else {
            "abandoned"
        };

        let session: Option<(Option<String>,)> =
            sqlx::query_as("SELECT session_id FROM commands WHERE id = ?")
                .bind(command_id)
                .fetch_optional(pool)
                .await?;
        let session_id = session
            .and_then(|(s,)| s)
            .and_then(|s| SessionId::from_str(&s).ok());

        let now = Utc::now().to_rfc3339();
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(
            "UPDATE pending_effects SET state = ?, resolved_at = ? WHERE id = ? AND state = ?",
        )
        .bind(new_state)
        .bind(&now)
        .bind(id)
        .bind(state)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            // Raced with another reconciler; leave its work intact.
            tx.rollback().await?;
            return Ok(false);
        }

        if let Some(session_id) = session_id {
            let sequence = next_sequence(&mut *tx, session_id).await?;
            append_event(
                &mut *tx,
                session_id,
                sequence,
                &Actor::System,
                &EventBody::NoteAppended {
                    text: format!("pending-effect {id} ({kind}) reconciled as {new_state}"),
                    run_id: None,
                },
                &now,
                None,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }
}

// --- free helpers ------------------------------------------------------------

/// Whether `role` may issue `body`. `Observer` may issue nothing (read-only);
/// `Contributor` may create/start/steer/submit; `Controller` additionally
/// controls runs and (as the most privileged role) resolves approvals;
/// `Approver` resolves approvals plus the contributor set. `AttachSession` and
/// `Unknown` are rejected before this check.
fn role_permits(role: ClientRole, body: &CommandBody) -> bool {
    use ClientRole::{Approver, Contributor, Controller};
    match body {
        CommandBody::CreateSession { .. }
        | CommandBody::StartRun { .. }
        | CommandBody::SubmitUserInput { .. }
        | CommandBody::QueueSteering { .. } => {
            matches!(role, Contributor | Controller | Approver)
        }
        CommandBody::CancelRun { .. }
        | CommandBody::PauseRun { .. }
        | CommandBody::ResumeRun { .. } => matches!(role, Controller),
        CommandBody::ResolveApproval { .. } => matches!(role, Approver | Controller),
        _ => false,
    }
}

/// The `internal.command-apply-failed` error every infrastructure (DB/serde)
/// failure collapses to — retryable, since a transient DB error may clear.
fn internal_error(err: impl std::fmt::Display) -> CodypendentError {
    CodypendentError::new("internal.command-apply-failed", err.to_string(), true)
}

fn run_not_found(run_id: RunId) -> CodypendentError {
    CodypendentError::new("protocol.run-not-found", format!("no run {run_id}"), false)
}

/// Whether a lifecycle command is legal from the run's current state.
///
/// Without this guard `ResumeRun` on a `Completed` run flipped the projection
/// back to `Running` with no executor attached — a zombie polluting
/// `active_runs` until the next boot's recovery force-failed it and appended
/// contradictory terminal events onto an already-finished run.
fn validate_run_transition(
    body: &CommandBody,
    run_id: RunId,
    state: RunState,
) -> Result<(), CodypendentError> {
    let terminal = matches!(
        state,
        RunState::Completed | RunState::Failed | RunState::Cancelled
    );
    let (verb, legal) = match body {
        // Cancelling is legal from any live state.
        CommandBody::CancelRun { .. } => ("cancel", !terminal && state != RunState::Unknown),
        // Pausing is legal from any live, not-already-paused state.
        CommandBody::PauseRun { .. } => (
            "pause",
            !terminal && !matches!(state, RunState::Paused | RunState::Unknown),
        ),
        // Resuming means "leave Paused" — anything else is already live or done.
        CommandBody::ResumeRun { .. } => ("resume", state == RunState::Paused),
        _ => ("transition", true),
    };
    if legal {
        Ok(())
    } else {
        Err(CodypendentError::new(
            "run.invalid-transition",
            format!("cannot {verb} run {run_id} in state {state:?}"),
            false,
        ))
    }
}

/// The [`RunState`]s from which `body`'s transition is legal, as a set the
/// write path can assert atomically via
/// [`projections::set_run_state_if_legal`] (FP-3). Derived directly from
/// [`validate_run_transition`] (evaluated against every known state) rather
/// than duplicating its rule as a second, hand-maintained list — so the two
/// can never drift apart.
fn legal_prior_states(body: &CommandBody, run_id: RunId) -> Vec<RunState> {
    const ALL_STATES: [RunState; 10] = [
        RunState::Queued,
        RunState::Preparing,
        RunState::Running,
        RunState::WaitingForApproval,
        RunState::WaitingForUserInput,
        RunState::Paused,
        RunState::Recovering,
        RunState::Completed,
        RunState::Failed,
        RunState::Cancelled,
    ];
    ALL_STATES
        .into_iter()
        .filter(|&state| validate_run_transition(body, run_id, state).is_ok())
        .collect()
}

fn approval_not_found(approval_id: codypendent_protocol::ApprovalId) -> CodypendentError {
    CodypendentError::new(
        "approval.not-found",
        format!("no approval {approval_id}"),
        false,
    )
}

/// The structured `protocol.revision-conflict` returned when a command's
/// `expected_revision` guard does not match the session's live revision. Not
/// retryable (an identical retry would carry the same stale revision).
fn revision_conflict(expected: u64, actual: u64) -> CodypendentError {
    CodypendentError::new(
        "protocol.revision-conflict",
        format!("expected session revision {expected} but it is at {actual}"),
        false,
    )
}

/// Whether `err` wraps a SQLite UNIQUE / PRIMARY KEY constraint violation — the
/// signal that a concurrent delivery of the same command won the race to insert
/// the `commands` row. Detected via the typed `sqlx` database error (not string
/// matching), so unrelated infrastructure failures are never mistaken for a
/// duplicate.
fn is_unique_violation(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<sqlx::Error>(),
        Some(sqlx::Error::Database(db)) if db.is_unique_violation()
    )
}

/// A failed `expected_revision` guard, carried out of the write transaction as a
/// downcastable error so the caller can surface it as `protocol.revision-conflict`
/// (distinct from an infrastructure failure).
#[derive(Debug)]
struct RevisionConflict {
    expected: u64,
    actual: u64,
}

impl std::fmt::Display for RevisionConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "expected session revision {} but it is at {}",
            self.expected, self.actual
        )
    }
}

impl std::error::Error for RevisionConflict {}

/// A run-state lifecycle transition that failed re-validation *inside* the
/// write transaction (FP-3) — carried out of [`commit`](CommandProcessor::commit)
/// as a downcastable error, exactly like [`RevisionConflict`], so
/// [`run_transaction`](CommandProcessor::run_transaction) can surface the SAME
/// structured [`CodypendentError`] `validate_run_transition`/`run_not_found`
/// would have produced, rather than a generic internal error.
#[derive(Debug)]
struct RunTransitionRejected(CodypendentError);

impl std::fmt::Display for RunTransitionRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.message)
    }
}

impl std::error::Error for RunTransitionRejected {}

/// The highest event sequence for a session, read inside the caller's tx (so it
/// reflects appends made earlier in the same transaction). `None` for a session
/// with no events yet. Used by the `ResolveApproval` no-op (already-resolved)
/// path to report a sensible `last_sequence`.
async fn tx_max_sequence(
    exec: impl sqlx::SqliteExecutor<'_>,
    session_id: SessionId,
) -> anyhow::Result<Option<u64>> {
    let (max,): (i64,) =
        sqlx::query_as("SELECT COALESCE(MAX(sequence), 0) FROM events WHERE session_id = ?")
            .bind(session_id.to_string())
            .fetch_one(exec)
            .await?;
    Ok(if max > 0 {
        Some(u64::try_from(max)?)
    } else {
        None
    })
}

/// Restated rejection for the `AttachSession`/`Unknown` arms of `apply` (already
/// rejected in `validate`; this keeps the dispatch match total).
fn rejected_for_body(body: &CommandBody) -> CodypendentError {
    match body {
        CommandBody::AttachSession { .. } => CodypendentError::new(
            "protocol.attach-is-connection-level",
            "AttachSession is handled by the connection layer, not the command write path",
            false,
        ),
        _ => CodypendentError::new("protocol.unsupported-payload", "unsupported command", false),
    }
}

fn map_approval_error(err: ApprovalError) -> CodypendentError {
    match err {
        ApprovalError::NotFound { .. } => {
            CodypendentError::new("approval.not-found", err.to_string(), false)
        }
        ApprovalError::AlreadyResolved { .. } => {
            CodypendentError::new("approval.already-resolved", err.to_string(), false)
        }
        ApprovalError::UnsupportedDecision | ApprovalError::UnsupportedScope => {
            CodypendentError::new("protocol.unsupported-payload", err.to_string(), false)
        }
        other => internal_error(other),
    }
}

/// The columns of a recorded command that idempotency handling needs.
struct ExistingCommand {
    command_id: CommandId,
    status: String,
    result_json: Option<String>,
    body: String,
    session_id: Option<SessionId>,
    client_id: String,
}

/// Raw row shape of [`lookup_command`]:
/// (id, status, result_json, body, session_id, client_id).
type CommandRow = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
);

async fn lookup_command(
    pool: &SqlitePool,
    idempotency_key: &str,
) -> anyhow::Result<Option<ExistingCommand>> {
    let row: Option<CommandRow> = sqlx::query_as(
        "SELECT id, status, result_json, body, session_id, client_id \
             FROM commands WHERE idempotency_key = ?",
    )
    .bind(idempotency_key)
    .fetch_optional(pool)
    .await?;

    match row {
        None => Ok(None),
        Some((id, status, result_json, body, session_id, client_id)) => Ok(Some(ExistingCommand {
            command_id: CommandId::from_str(&id)?,
            status,
            result_json,
            body,
            session_id: session_id.map(|s| SessionId::from_str(&s)).transpose()?,
            client_id,
        })),
    }
}

async fn finalize_applied(
    pool: &SqlitePool,
    command_id: CommandId,
    outcome: &CommandOutcome,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE commands SET status = 'applied', result_json = ?, applied_at = ? WHERE id = ?",
    )
    .bind(serde_json::to_string(outcome)?)
    .bind(Utc::now().to_rfc3339())
    .bind(command_id.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

async fn session_exists(pool: &SqlitePool, session_id: SessionId) -> anyhow::Result<bool> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM sessions WHERE id = ?")
        .bind(session_id.to_string())
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

async fn approval_session(
    pool: &SqlitePool,
    approval_id: codypendent_protocol::ApprovalId,
) -> anyhow::Result<Option<SessionId>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT r.session_id FROM approvals a JOIN runs r ON a.run_id = r.id WHERE a.id = ?",
    )
    .bind(approval_id.to_string())
    .fetch_optional(pool)
    .await?;
    row.map(|(s,)| SessionId::from_str(&s))
        .transpose()
        .map_err(Into::into)
}

async fn max_sequence(pool: &SqlitePool, session_id: SessionId) -> anyhow::Result<Option<u64>> {
    let (max,): (i64,) =
        sqlx::query_as("SELECT COALESCE(MAX(sequence), 0) FROM events WHERE session_id = ?")
            .bind(session_id.to_string())
            .fetch_one(pool)
            .await?;
    Ok(if max > 0 {
        Some(u64::try_from(max)?)
    } else {
        None
    })
}

/// The next 1-based event sequence for a session, read inside the caller's tx so
/// the append that claims it is atomic with the read (mirrors
/// [`crate::approvals`]).
async fn next_sequence(
    exec: impl sqlx::SqliteExecutor<'_>,
    session_id: SessionId,
) -> Result<i64, sqlx::Error> {
    let (max,): (i64,) =
        sqlx::query_as("SELECT COALESCE(MAX(sequence), 0) FROM events WHERE session_id = ?")
            .bind(session_id.to_string())
            .fetch_one(exec)
            .await?;
    Ok(max + 1)
}

/// Append one event within the caller's transaction, stamping `causation_id`
/// with the command that produced it (unlike the approval broker's helper, which
/// leaves causation null for its own housekeeping events).
async fn append_event(
    exec: impl sqlx::SqliteExecutor<'_>,
    session_id: SessionId,
    sequence: i64,
    actor: &Actor,
    body: &EventBody,
    occurred_at: &str,
    causation_id: Option<CommandId>,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO events \
         (session_id, sequence, occurred_at, actor, body, causation_id, correlation_id, schema_version) \
         VALUES (?, ?, ?, ?, ?, ?, NULL, 1)",
    )
    .bind(session_id.to_string())
    .bind(sequence)
    .bind(occurred_at)
    .bind(serde_json::to_string(actor)?)
    .bind(serde_json::to_string(body)?)
    .bind(causation_id.map(|id| id.to_string()))
    .execute(exec)
    .await?;
    Ok(())
}

/// An optional row to insert before a command's events (only `CreateSession`
/// needs one, for the events FK).
enum PreInsert<'a> {
    None,
    Session {
        session_id: SessionId,
        title: &'a str,
    },
}

/// How a command's write transaction handles `sessions.revision` (STEP 1.3
/// optimistic concurrency).
enum RevisionOp {
    /// The command creates the session now (`CreateSession`): it is inserted at
    /// revision 0 and `expected_revision` is ignored (no prior session to guard).
    Establish,
    /// The command mutates an existing session's state: check `expected` (when
    /// `Some`) against the live revision inside the tx, then advance it by one.
    Bump { expected: Option<u64> },
}

/// The projection mutation a command performs inside its transaction.
enum ProjectionOp {
    None,
    InsertRun {
        run_id: RunId,
        session_id: SessionId,
        objective: String,
        mode: AgentMode,
    },
    SetRunState {
        run_id: RunId,
        state: RunState,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::{AgentMode, ApprovalDecision, ApprovalScope};
    use std::path::Path;
    use tempfile::tempdir;

    async fn test_pool(dir: &Path) -> SqlitePool {
        crate::db::open_database(&dir.join("test.db"))
            .await
            .expect("open database")
    }

    fn ctx(role: ClientRole) -> ApplyContext {
        ApplyContext {
            client_id: ClientId::new(),
            role,
        }
    }

    fn command(body: CommandBody, key: &str) -> Command {
        Command {
            command_id: CommandId::new(),
            idempotency_key: key.to_string(),
            expected_revision: None,
            body,
        }
    }

    async fn create_session(
        processor: &CommandProcessor,
        pool: &SqlitePool,
        key: &str,
    ) -> SessionId {
        let outcome = processor
            .apply(
                pool,
                ctx(ClientRole::Contributor),
                command(
                    CommandBody::CreateSession {
                        workspace: codypendent_protocol::WorkspaceId::new(),
                        title: "diagnose the failing test".to_string(),
                    },
                    key,
                ),
            )
            .await
            .expect("create session");
        outcome.created_session.expect("session id in outcome")
    }

    async fn run_count(pool: &SqlitePool, session_id: SessionId) -> i64 {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM runs WHERE session_id = ?")
            .bind(session_id.to_string())
            .fetch_one(pool)
            .await
            .expect("count runs");
        count
    }

    #[tokio::test]
    async fn duplicate_command_is_idempotent() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "idem-create").await;

        let start = command(
            CommandBody::StartRun {
                session_id: session,
                objective: "fix it".to_string(),
                mode: AgentMode::Build,
                repository: None,
            },
            "idem-start",
        );

        // The SAME envelope, delivered twice.
        let first = processor
            .apply(&pool, ctx(ClientRole::Contributor), start.clone())
            .await
            .expect("first apply");
        let second = processor
            .apply(&pool, ctx(ClientRole::Contributor), start.clone())
            .await
            .expect("second apply");

        // The first delivery freshly applies; the duplicate replays. That
        // distinction (never sent to the client) is what makes the server launch
        // the executor exactly once, while the user-facing outcome is identical.
        assert!(first.newly_applied, "first delivery is a fresh application");
        assert!(!second.newly_applied, "duplicate delivery is a replay");
        assert_eq!(
            CommandOutcome {
                newly_applied: false,
                ..first.clone()
            },
            second,
            "idempotent replay returns the same (user-facing) outcome"
        );
        assert_eq!(run_count(&pool, session).await, 1, "exactly one run row");
        assert!(first.created_run.is_some());
    }

    #[tokio::test]
    async fn create_session_then_start_run_projects() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();

        let session = create_session(&processor, &pool, "create").await;

        // Subscribe before the run's events are published.
        let mut rx = processor.subscriptions().subscribe(session);

        let run = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::StartRun {
                        session_id: session,
                        objective: "diagnose".to_string(),
                        mode: AgentMode::Build,
                        repository: None,
                    },
                    "start",
                ),
            )
            .await
            .expect("start run")
            .created_run
            .expect("run id");

        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::SubmitUserInput {
                        session_id: session,
                        text: "focus on the parser".to_string(),
                        mode: AgentMode::Build,
                    },
                    "input",
                ),
            )
            .await
            .expect("submit input");

        // Projection: the run row exists in Queued, and the snapshot lists it
        // as active.
        assert_eq!(
            projections::load_run_state(&pool, run).await.unwrap(),
            Some(RunState::Queued),
        );
        let projection = projections::session_projection(&pool, session)
            .await
            .unwrap();
        assert!(projection.active_runs.contains(&run));
        assert_eq!(projection.title, "diagnose the failing test");

        // Published events arrive in order: RunStarted then NoteAppended.
        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        assert!(matches!(first.body, EventBody::RunStarted { .. }));
        assert!(matches!(second.body, EventBody::NoteAppended { .. }));
        assert!(first.sequence < second.sequence);
    }

    #[tokio::test]
    async fn observer_cannot_start_run() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "create").await;

        let err = processor
            .apply(
                &pool,
                ctx(ClientRole::Observer),
                command(
                    CommandBody::StartRun {
                        session_id: session,
                        objective: "diagnose".to_string(),
                        mode: AgentMode::Build,
                        repository: None,
                    },
                    "start",
                ),
            )
            .await
            .expect_err("observer must be denied");

        assert_eq!(err.code, "protocol.role-denied");
        assert_eq!(run_count(&pool, session).await, 0, "no run row created");
    }

    #[tokio::test]
    async fn crash_between_persist_and_effect() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "create").await;

        // Simulate a crash mid-apply: a command left `received` with an
        // `intended` pending effect that never ran.
        let command_id = CommandId::new();
        sqlx::query(
            "INSERT INTO commands \
             (id, idempotency_key, session_id, client_id, body, status, received_at) \
             VALUES (?, 'crashed', ?, 'client', '{\"type\":\"SubmitUserInput\"}', 'received', ?)",
        )
        .bind(command_id.to_string())
        .bind(session.to_string())
        .bind(Utc::now().to_rfc3339())
        .execute(&pool)
        .await
        .unwrap();

        let effect_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO pending_effects (id, command_id, kind, intent_json, state, created_at) \
             VALUES (?, ?, 'shell', '{}', 'intended', ?)",
        )
        .bind(&effect_id)
        .bind(command_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .execute(&pool)
        .await
        .unwrap();

        let reconciled = processor.reconcile_pending_effects(&pool).await.unwrap();
        assert_eq!(reconciled, 1);

        // The effect ended reconciled/abandoned — exactly once, no duplicate.
        let (state,): (String,) = sqlx::query_as("SELECT state FROM pending_effects WHERE id = ?")
            .bind(&effect_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            state == "abandoned" || state == "reconciled",
            "unexpected state {state}"
        );
        let (effect_rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pending_effects")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(effect_rows, 1, "no second effect row appeared");
    }

    #[tokio::test]
    async fn lifecycle_commands_validate_run_state() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();

        let session = create_session(&processor, &pool, "create").await;
        let run = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::StartRun {
                        session_id: session,
                        objective: "diagnose".to_string(),
                        mode: AgentMode::Build,
                        repository: None,
                    },
                    "start",
                ),
            )
            .await
            .unwrap()
            .created_run
            .unwrap();

        // Resuming a run that is not paused is refused.
        let err = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(CommandBody::ResumeRun { run_id: run }, "resume-live"),
            )
            .await
            .expect_err("resuming a non-paused run must be rejected");
        assert_eq!(err.code, "run.invalid-transition");

        // Cancel is legal from a live state…
        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(CommandBody::CancelRun { run_id: run }, "cancel-1"),
            )
            .await
            .unwrap();

        // …but resuming (or re-cancelling) a terminal run must be refused: the
        // old behavior flipped a Completed/Cancelled run back to `Running` with
        // no executor attached — a zombie in `active_runs` until next boot.
        let err = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(CommandBody::ResumeRun { run_id: run }, "resume-done"),
            )
            .await
            .expect_err("resuming a cancelled run must be rejected");
        assert_eq!(err.code, "run.invalid-transition");
        let err = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(CommandBody::CancelRun { run_id: run }, "cancel-2"),
            )
            .await
            .expect_err("cancelling an already-cancelled run must be rejected");
        assert_eq!(err.code, "run.invalid-transition");
    }

    #[tokio::test]
    async fn set_run_state_if_legal_refuses_a_transition_past_a_stale_prior_state() {
        // FP-3, a direct store-level pin of the atomic conditional write: the
        // guard must refuse to apply a transition once the run's CURRENT state
        // is no longer in the legal set — even though, at some earlier moment
        // (mirroring a stale pre-transaction `validate()` read), it would have
        // been legal. This is the primitive that closes the check-then-act
        // race without needing to orchestrate real concurrency.
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "create").await;
        let run = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::StartRun {
                        session_id: session,
                        objective: "diagnose".to_string(),
                        mode: AgentMode::Build,
                        repository: None,
                    },
                    "start",
                ),
            )
            .await
            .unwrap()
            .created_run
            .unwrap();

        // The run is `Queued` (StartRun's initial projection) — both Cancel and
        // Pause are legal from Queued.
        let cancel_legal = legal_prior_states(&CommandBody::CancelRun { run_id: run }, run);
        let pause_legal = legal_prior_states(&CommandBody::PauseRun { run_id: run }, run);

        // The first write (the WINNING racer) succeeds and lands the run in a
        // state (`Cancelled`) from which Pause is no longer legal.
        let affected =
            projections::set_run_state_if_legal(&pool, run, &cancel_legal, RunState::Cancelled)
                .await
                .unwrap();
        assert_eq!(affected, 1, "the first (winning) transition applies");

        // The second write (the LOSING racer, whose `legal_from` was computed
        // against the STALE `Queued` read) must now be refused: the run's
        // ACTUAL current state (`Cancelled`) is not in `pause_legal`.
        let affected =
            projections::set_run_state_if_legal(&pool, run, &pause_legal, RunState::Paused)
                .await
                .unwrap();
        assert_eq!(affected, 0, "the second (losing) transition must not apply");

        // The run is still `Cancelled` — never resurrected to `Paused`.
        assert_eq!(
            projections::load_run_state(&pool, run).await.unwrap(),
            Some(RunState::Cancelled)
        );
    }

    #[tokio::test]
    async fn a_write_whose_validation_is_now_stale_cannot_commit() {
        // FP-3, deterministic reproduction of the exact race window (rather
        // than relying on real thread scheduling to interleave two `apply()`
        // calls, which is inherently non-deterministic — a `tokio::join!`
        // version of this test was tried and only reproduced the bug on
        // roughly half of runs even on a multi-threaded runtime): a command
        // whose `validate()` already passed (against the run's state at read
        // time) reaches its write stage — modeled here by calling the private
        // write-path method directly, exactly the state a command is in
        // between `validate()` returning `Ok` and `commit()` running — but by
        // then a DIFFERENT command has already taken the run to a state from
        // which this one is no longer legal. The write itself must refuse,
        // not blindly apply what an earlier read decided.
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "create").await;
        let run = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::StartRun {
                        session_id: session,
                        objective: "diagnose".to_string(),
                        mode: AgentMode::Build,
                        repository: None,
                    },
                    "start",
                ),
            )
            .await
            .unwrap()
            .created_run
            .unwrap();

        // The WINNING command commits first, taking the run terminal.
        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(CommandBody::CancelRun { run_id: run }, "winner-cancel"),
            )
            .await
            .expect("cancel applies");
        assert_eq!(
            projections::load_run_state(&pool, run).await.unwrap(),
            Some(RunState::Cancelled)
        );

        // The LOSING command reaches its write stage as if its OWN `validate()`
        // had already passed against the run's PRE-cancellation state (calling
        // the write-path method directly skips `apply()`'s own validate call,
        // modeling exactly that moment).
        let pause_command = command(CommandBody::PauseRun { run_id: run }, "loser-pause");
        let err = processor
            .apply_run_state(
                &pool,
                &ctx(ClientRole::Controller),
                &pause_command,
                run,
                RunState::Paused,
            )
            .await
            .expect_err("the write must re-validate against the CURRENT state and refuse");
        assert_eq!(err.code, "run.invalid-transition");

        // The run is still `Cancelled` — never resurrected to `Paused`.
        assert_eq!(
            projections::load_run_state(&pool, run).await.unwrap(),
            Some(RunState::Cancelled)
        );
    }

    #[tokio::test]
    async fn replay_is_deterministic() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();

        let session = create_session(&processor, &pool, "create").await;
        let run = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::StartRun {
                        session_id: session,
                        objective: "diagnose".to_string(),
                        mode: AgentMode::Build,
                        repository: None,
                    },
                    "start",
                ),
            )
            .await
            .unwrap()
            .created_run
            .unwrap();
        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(CommandBody::PauseRun { run_id: run }, "pause"),
            )
            .await
            .unwrap();
        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::SubmitUserInput {
                        session_id: session,
                        text: "keep going".to_string(),
                        mode: AgentMode::Build,
                    },
                    "input",
                ),
            )
            .await
            .unwrap();

        // Fold the ledger events into the projection by hand, and assert it
        // equals the DB-backed projection: derived state is deterministic.
        let events = crate::ledger::load_events(&pool, session).await.unwrap();
        let mut title = String::new();
        let mut closed = false;
        let mut active: Vec<RunId> = Vec::new();
        let mut last_sequence = 0u64;
        for event in &events {
            last_sequence = event.sequence;
            match &event.body {
                EventBody::SessionCreated { title: t } => title = t.clone(),
                EventBody::SessionClosed => closed = true,
                EventBody::RunStarted { run_id, .. } => active.push(*run_id),
                EventBody::RunStateChanged { run_id, state }
                    if projections::is_terminal(*state) =>
                {
                    active.retain(|r| r != run_id);
                }
                _ => {}
            }
        }
        active.sort();
        let folded = codypendent_protocol::SessionProjection {
            session_id: session,
            title,
            last_sequence,
            active_runs: active,
            closed,
        };

        let projected = projections::session_projection(&pool, session)
            .await
            .unwrap();
        assert_eq!(folded, projected);
    }

    #[tokio::test]
    async fn unknown_command_is_rejected_structurally() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();

        let err = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(CommandBody::Unknown, "unknown"),
            )
            .await
            .expect_err("unknown body rejected");
        assert_eq!(err.code, "protocol.unsupported-payload");
    }

    #[tokio::test]
    async fn attach_session_is_rejected_by_the_write_path() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();

        let err = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::AttachSession {
                        session_id: SessionId::new(),
                        last_seen_sequence: None,
                        subscriptions: vec![],
                        requested_role: ClientRole::Observer,
                    },
                    "attach",
                ),
            )
            .await
            .expect_err("attach rejected");
        assert_eq!(err.code, "protocol.attach-is-connection-level");
    }

    #[tokio::test]
    async fn resolve_approval_delegates_to_the_broker() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let broker = ApprovalBroker::new();
        let processor = CommandProcessor::new(SubscriptionHub::new(), broker.clone());
        let session = create_session(&processor, &pool, "create").await;

        // Seed a run + a pending approval to resolve.
        let run = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::StartRun {
                        session_id: session,
                        objective: "diagnose".to_string(),
                        mode: AgentMode::Build,
                        repository: None,
                    },
                    "start",
                ),
            )
            .await
            .unwrap()
            .created_run
            .unwrap();
        let approval_id = broker
            .request(
                &pool,
                session,
                run,
                codypendent_protocol::ProposedAction::ExecuteCommand {
                    program: "cargo".to_string(),
                    args: vec!["test".to_string()],
                    environment: Vec::new(),
                    cwd: None,
                },
                codypendent_protocol::Risk {
                    level: codypendent_protocol::RiskLevel::Medium,
                    reasons: vec![],
                },
                vec![],
                None,
            )
            .await
            .unwrap();

        let mut rx = processor.subscriptions().subscribe(session);
        processor
            .apply(
                &pool,
                ctx(ClientRole::Approver),
                command(
                    CommandBody::ResolveApproval {
                        approval_id,
                        decision: ApprovalDecision::Approve,
                        scope: ApprovalScope::Once,
                    },
                    "resolve",
                ),
            )
            .await
            .expect("resolve approval");

        let (state,): (String,) = sqlx::query_as("SELECT state FROM approvals WHERE id = ?")
            .bind(approval_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "approved");

        // The processor re-published the broker's ApprovalResolved.
        let event = rx.recv().await.unwrap();
        assert!(matches!(
            event.body,
            EventBody::ApprovalResolved {
                decision: ApprovalDecision::Approve,
                ..
            }
        ));
    }

    /// issue #6 item 2b: the `expected_revision` guard and the revision bump are
    /// held in the same transaction as the `ApprovalResolved` append, so a resolve
    /// consumes exactly one revision and a second command carrying the now-stale
    /// revision is rejected instead of also passing.
    #[tokio::test]
    async fn resolve_approval_guards_and_bumps_the_session_revision() {
        use codypendent_protocol::{ProposedAction, Risk, RiskLevel};

        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let broker = ApprovalBroker::new();
        let processor = CommandProcessor::new(SubscriptionHub::new(), broker.clone());
        let session = create_session(&processor, &pool, "create").await;

        let run = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::StartRun {
                        session_id: session,
                        objective: "diagnose".to_string(),
                        mode: AgentMode::Build,
                        repository: None,
                    },
                    "start",
                ),
            )
            .await
            .unwrap()
            .created_run
            .unwrap();

        // Two distinct pending approvals in this session.
        let request = |program: &'static str| {
            let broker = broker.clone();
            let pool = pool.clone();
            async move {
                broker
                    .request(
                        &pool,
                        session,
                        run,
                        ProposedAction::ExecuteCommand {
                            program: program.to_string(),
                            args: vec![],
                            environment: Vec::new(),
                            cwd: None,
                        },
                        Risk {
                            level: RiskLevel::Medium,
                            reasons: vec![],
                        },
                        vec![],
                        None,
                    )
                    .await
                    .unwrap()
            }
        };
        let a1 = request("cargo").await;
        let a2 = request("git").await;

        let revision = |pool: SqlitePool| async move {
            let (r,): (i64,) = sqlx::query_as("SELECT revision FROM sessions WHERE id = ?")
                .bind(session.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
            u64::try_from(r).unwrap()
        };
        let rev = revision(pool.clone()).await;

        let resolve_cmd = |approval, key: &str, expected| {
            let mut cmd = command(
                CommandBody::ResolveApproval {
                    approval_id: approval,
                    decision: ApprovalDecision::Approve,
                    scope: ApprovalScope::Once,
                },
                key,
            );
            cmd.expected_revision = expected;
            cmd
        };

        // Resolve a1 at the current revision → applies and bumps by one.
        processor
            .apply(
                &pool,
                ctx(ClientRole::Approver),
                resolve_cmd(a1, "r1", Some(rev)),
            )
            .await
            .expect("first resolve applies");
        assert_eq!(
            revision(pool.clone()).await,
            rev + 1,
            "resolving bumped the session revision"
        );

        // Resolve a2 carrying the stale revision → rejected, a2 untouched.
        let err = processor
            .apply(
                &pool,
                ctx(ClientRole::Approver),
                resolve_cmd(a2, "r2", Some(rev)),
            )
            .await
            .expect_err("a stale expected_revision is rejected");
        assert_eq!(err.code, "protocol.revision-conflict");
        let (state,): (String,) = sqlx::query_as("SELECT state FROM approvals WHERE id = ?")
            .bind(a2.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "pending", "the rejected command applied nothing");

        // Resolve a2 at the fresh revision → applies.
        processor
            .apply(
                &pool,
                ctx(ClientRole::Approver),
                resolve_cmd(a2, "r3", Some(rev + 1)),
            )
            .await
            .expect("resolve at the fresh revision applies");
    }
}
