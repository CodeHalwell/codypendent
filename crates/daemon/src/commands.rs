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

use std::path::Path;
use std::str::FromStr;

use chrono::Utc;
use codypendent_protocol::{
    Actor, AgentMode, AnalyticsBudgetRequest, ApprovalDecision, ApprovalScope, ArtifactRef,
    AutomationBindingRequest, ClientId, ClientRole, CodypendentError, Command, CommandBody,
    CommandId, DataClassification, EventBody, ModelId, PromptDelivery, PromptId, QuestionId,
    QuestionOutcome, RepositoryId, RunId, RunState, SessionDeletionMode, SessionEvent, SessionId,
    SessionLifecycleAction, SessionSummary, WorkspaceId,
};
use serde::{Deserialize, Serialize};
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};

use crate::approvals::{ApprovalBroker, ApprovalError};
use crate::artifacts::ArtifactStore;
use crate::principal::PeerPrincipal;
use crate::projections;
use crate::questions::{QuestionBroker, QuestionError, QuestionReply};
use codypendent_sandbox::executor::{
    enforcing_executor, SandboxCommand, SandboxError, SandboxExecutor, SandboxOutcome,
};
use codypendent_sandbox::profile::{SandboxProfile, ENV_ALLOWLIST};

use crate::server::resolve_run_repository;
use crate::subscriptions::SubscriptionHub;

/// Wall-clock ceiling for a `!` operator shell escape — the sandbox kills the
/// process group past this. Matches the `shell.run` tool's 30s default so a
/// `!tail -f` cannot hang the write path.
const USER_SHELL_WALL_SECONDS: u64 = 30;
/// CPU-time ceiling for a `!` command (rlimit).
const USER_SHELL_CPU_SECONDS: u64 = 30;
/// Memory ceiling for a `!` command (rlimit), in MiB.
const USER_SHELL_MEMORY_MB: u64 = 512;
/// Captured-output ceiling for a `!` command, in MiB — the sandbox truncates
/// beyond this, so a chatty command cannot bloat the ledger note.
const USER_SHELL_OUTPUT_MB: u64 = 1;

/// Durable, user-legible reason used when a run cannot safely recover the
/// repository/model provenance that determines where and how it executes.
/// Raw database errors stay in logs; the ledger records this stable message.
pub const RUN_PROVENANCE_FAILURE_REASON: &str =
    "run was not started because its session repository could not be resolved safely";

/// A run's resolved model policy is not carried by the Phase 1 `StartRun`
/// command; the write path records this default (a `models.toml` profile id).
const DEFAULT_MODEL_POLICY: &str = "hosted-default";
/// Likewise the run budget: an empty JSON object until the agent loop sets one.
const DEFAULT_BUDGET_JSON: &str = "{}";

/// Who is issuing a command, for validation and event attribution.
///
/// Three separate things, deliberately not conflated:
///
/// * `principal` — **who you are**, derived by the server from the connection's
///   peer credentials ([`PeerPrincipal`]). This is what authorizes access to a
///   session and what every `Actor::Human` is minted from.
/// * `role` — **what you asked to be limited to**. A client-supplied assertion
///   that can only narrow (see [`role_permits`]); it never widens what the
///   principal may reach.
/// * `client_id` — **which connection this is**, for correlation: the
///   `commands` row, presence, and event attribution. Not authority.
#[derive(Debug, Clone)]
pub struct ApplyContext {
    pub client_id: ClientId,
    pub role: ClientRole,
    pub principal: PeerPrincipal,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum LifecycleResponse {
    Summary(Box<SessionSummary>),
    Deleted {
        session_id: SessionId,
        tombstoned: bool,
    },
    Exported {
        artifact: ArtifactRef,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedLifecycleOutcome {
    #[serde(flatten)]
    outcome: CommandOutcome,
    lifecycle_response: LifecycleResponse,
}

pub(crate) async fn lifecycle_response(
    pool: &SqlitePool,
    idempotency_key: &str,
) -> Result<LifecycleResponse, CodypendentError> {
    let existing = lookup_command(pool, idempotency_key)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| internal_error("applied lifecycle command disappeared"))?;
    let json = existing
        .result_json
        .ok_or_else(|| internal_error("applied lifecycle command is missing result_json"))?;
    serde_json::from_str::<PersistedLifecycleOutcome>(&json)
        .map(|persisted| persisted.lifecycle_response)
        .map_err(internal_error)
}

/// Applies commands through the crash-consistent write path, owning the shared
/// [`SubscriptionHub`] it publishes to and the [`ApprovalBroker`] / [`QuestionBroker`]
/// it delegates resolutions to. Cloning shares all three (each is `Arc`-backed).
#[derive(Debug, Clone, Default)]
pub struct CommandProcessor {
    subscriptions: SubscriptionHub,
    approvals: ApprovalBroker,
    questions: QuestionBroker,
    artifacts: Option<ArtifactStore>,
}

impl CommandProcessor {
    /// A processor wired to a shared subscription hub, approval broker, and question broker.
    pub fn new(
        subscriptions: SubscriptionHub,
        approvals: ApprovalBroker,
        questions: QuestionBroker,
    ) -> Self {
        Self {
            subscriptions,
            approvals,
            questions,
            artifacts: None,
        }
    }

    /// A processor wired to subscriptions, brokers, and an artifact store.
    pub fn with_artifacts(
        subscriptions: SubscriptionHub,
        approvals: ApprovalBroker,
        questions: QuestionBroker,
        artifacts: ArtifactStore,
    ) -> Self {
        Self {
            subscriptions,
            approvals,
            questions,
            artifacts: Some(artifacts),
        }
    }

    pub fn set_artifacts(&mut self, artifacts: ArtifactStore) {
        self.artifacts = Some(artifacts);
    }

    pub fn artifacts(&self) -> Option<&ArtifactStore> {
        self.artifacts.as_ref()
    }

    fn artifacts_or_default(&self) -> ArtifactStore {
        self.artifacts.clone().unwrap_or_else(|| {
            ArtifactStore::new(std::env::temp_dir().join("codypendent_artifacts"))
        })
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

    /// The question broker this processor delegates `ResolveQuestion` to.
    pub fn questions(&self) -> &QuestionBroker {
        &self.questions
    }

    /// Replay an already-recorded command outcome without admitting a new
    /// command. The server uses this before an expensive external input
    /// preprocessing step (voice transcription): a lost reply must not send the
    /// same audio off-device again merely to discover that the write path had
    /// already applied its idempotency key.
    pub async fn replay_existing(
        &self,
        pool: &SqlitePool,
        idempotency_key: &str,
    ) -> Result<Option<CommandOutcome>, CodypendentError> {
        let existing = lookup_command(pool, idempotency_key)
            .await
            .map_err(internal_error)?;
        match existing {
            Some(existing) => self.handle_existing(pool, existing).await.map(Some),
            None => Ok(None),
        }
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
        if is_reserved_unsupported_command(&command.body) {
            return Err(reserved_unsupported_error());
        }

        // Step 1: idempotency check FIRST.
        if let Some(existing) = lookup_command(pool, &command.idempotency_key)
            .await
            .map_err(internal_error)?
        {
            reject_replay_mismatch(&command.body, &existing.body)?;
            return self.handle_existing(pool, existing).await;
        }

        // Step 2: validate (schema, existence, role).
        self.validate(pool, &ctx, &command).await?;

        // Steps 3-6 per variant.
        match command.body.clone() {
            CommandBody::CreateSession {
                workspace,
                title,
                repository,
                internal,
                parent_session_id,
                parent_run_id,
            } => {
                self.apply_create_session(
                    pool,
                    &ctx,
                    &command,
                    workspace,
                    title,
                    repository,
                    internal,
                    parent_session_id,
                    parent_run_id,
                )
                .await
            }
            CommandBody::CloseSession { session_id } => {
                self.apply_close_session(pool, &ctx, &command, session_id)
                    .await
            }
            CommandBody::MutateSessionLifecycle { session_id, action } => {
                self.apply_session_lifecycle(pool, &ctx, &command, session_id, action)
                    .await
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
                session_id,
                text,
                mode,
                // `model` (a mid-conversation pin) is persisted verbatim on the
                // command body — exactly like `StartRun.model` — and recovered by
                // `session_run_provenance` / `run_launch_provenance`, not written
                // by the projection path. The ledger row is the same either way.
                ..
            } => {
                self.apply_submit_input(pool, &ctx, &command, session_id, text, mode)
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
            CommandBody::ResolveQuestion {
                question_id,
                outcome,
            } => {
                self.apply_resolve_question(pool, &ctx, &command, question_id, outcome)
                    .await
            }
            CommandBody::QueuePrompt {
                session_id,
                text,
                mode,
                delivery,
            } => {
                self.apply_queue_prompt(pool, &ctx, &command, session_id, text, mode, delivery)
                    .await
            }
            CommandBody::UpdateQueuedPrompt {
                session_id,
                prompt_id,
                text,
                delivery,
            } => {
                self.apply_update_queued_prompt(
                    pool, &ctx, &command, session_id, prompt_id, text, delivery,
                )
                .await
            }
            CommandBody::PromoteQueuedPrompt {
                session_id,
                prompt_id,
            } => {
                self.apply_promote_queued_prompt(pool, &ctx, &command, session_id, prompt_id)
                    .await
            }
            CommandBody::DeleteQueuedPrompt {
                session_id,
                prompt_id,
            } => {
                self.apply_delete_queued_prompt(pool, &ctx, &command, session_id, prompt_id)
                    .await
            }
            CommandBody::RunUserShell {
                session_id,
                command: shell_cmd,
            } => {
                self.apply_run_user_shell(pool, &ctx, &command, session_id, shell_cmd)
                    .await
            }
            CommandBody::RememberMemory { session_id, text } => {
                self.apply_remember_memory(pool, &ctx, &command, session_id, text)
                    .await
            }
            CommandBody::RunEditorAction {
                session_id,
                action,
                context,
                model: _,
            } => {
                self.apply_run_editor_action(pool, &ctx, &command, session_id, action, context)
                    .await
            }
            CommandBody::ExportBundle { request } => {
                self.apply_export_bundle(pool, &ctx, &command, &request)
                    .await
            }
            CommandBody::ExportAnalytics { request } => {
                self.apply_export_analytics(pool, &ctx, &command, &request)
                    .await
            }
            CommandBody::ImportBundle { request } => {
                self.apply_import_bundle(pool, &ctx, &command, &request)
                    .await
            }
            CommandBody::MutateInbox { mutation } => {
                self.apply_mutate_inbox(pool, &ctx, &command, mutation)
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

        // `ForkSession` has an external effect (creating the forked session) that
        // is NOT folded into the command transaction. Complete it idempotently on
        // resume — mirroring how `ResolveApproval`'s effect is re-driven below —
        // so a fork whose `applied` finalize was skipped by a crash is finished on
        // recovery/retry, returning the SAME forked session id rather than leaving
        // it permanently `fork.in-progress`.
        if let CommandBody::ForkSession {
            session_id,
            checkpoint,
            name,
        } = &body
        {
            if let Some(outcome) = self
                .resume_fork(pool, &existing, *session_id, *checkpoint, name.clone())
                .await?
            {
                return Ok(outcome);
            }
        }

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
        } else if let CommandBody::ResolveQuestion {
            question_id,
            outcome,
        } = body
        {
            match self
                .questions
                .resolve(pool, question_id, outcome, existing.client_id.clone())
                .await
            {
                Ok(event) => {
                    if let Some(session_id) = existing.session_id {
                        self.subscriptions.publish(session_id, event);
                    }
                }
                Err(QuestionError::AlreadyResolved { .. }) => {}
                Err(e) => return Err(map_question_error(e)),
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

    /// Complete a `ForkSession` reservation whose `applied` finalize was skipped
    /// by a crash. The forked session id was pre-recorded on the reservation's
    /// `result_json`, so this re-drives the (idempotent, atomic) fork with that
    /// SAME id and finalizes `applied` — recovery returns the same forked session,
    /// never a second fork and never a permanent `fork.in-progress`.
    ///
    /// Returns `Ok(None)` when the reservation carries no recorded fork id (a
    /// legacy row), so the caller falls back to the generic finalize.
    async fn resume_fork(
        &self,
        pool: &SqlitePool,
        existing: &ExistingCommand,
        session_id: SessionId,
        checkpoint: codypendent_protocol::CheckpointId,
        name: Option<String>,
    ) -> Result<Option<CommandOutcome>, CodypendentError> {
        let recorded: Option<CommandOutcome> = existing
            .result_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok());
        let Some(fork_id) = recorded
            .as_ref()
            .and_then(|outcome| outcome.created_session)
        else {
            return Ok(None);
        };

        let checkpoint_row = crate::worktrees::fetch_checkpoint(pool, checkpoint)
            .await
            .map_err(internal_error)?;
        if let Some(checkpoint_row) = checkpoint_row {
            let owner_uid = session_owner_uid(pool, session_id)
                .await
                .map_err(internal_error)?;
            // Idempotent + atomic: if the fork already committed this returns the
            // same id immediately; otherwise it re-drives the whole fork.
            crate::forks::fork_session(pool, session_id, checkpoint_row, name, owner_uid, fork_id)
                .await?;
        } else if !crate::ledger::session_exists(pool, fork_id)
            .await
            .map_err(internal_error)?
        {
            // The checkpoint is gone AND the fork never committed — it cannot be
            // completed now. Leave the reservation `received` and reject retryably
            // rather than fabricate an outcome for a session that does not exist.
            return Err(CodypendentError::new(
                "fork.in-progress",
                "fork cannot be completed on recovery: checkpoint is no longer available",
                true,
            ));
        }

        let outcome = CommandOutcome {
            command_id: existing.command_id,
            created_session: Some(fork_id),
            created_run: None,
            last_sequence: recorded.and_then(|outcome| outcome.last_sequence),
            newly_applied: false,
        };
        finalize_applied(pool, existing.command_id, &outcome)
            .await
            .map_err(internal_error)?;
        Ok(Some(outcome))
    }

    // --- validation -----------------------------------------------------------

    async fn validate(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
    ) -> Result<(), CodypendentError> {
        if is_reserved_unsupported_command(&command.body) {
            return Err(reserved_unsupported_error());
        }

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
            | CommandBody::SubmitUserInput { session_id, .. }
            | CommandBody::CloseSession { session_id }
            | CommandBody::MutateSessionLifecycle { session_id, .. } => {
                if !active_session_exists(pool, *session_id)
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
            CommandBody::QueuePrompt {
                session_id, text, ..
            } => {
                if text.trim().is_empty() {
                    return Err(CodypendentError::new(
                        "prompt-queue.empty",
                        "queued prompt text cannot be empty",
                        false,
                    ));
                }
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
            CommandBody::UpdateQueuedPrompt {
                session_id, text, ..
            } => {
                if text.as_ref().is_some_and(|t| t.trim().is_empty()) {
                    return Err(CodypendentError::new(
                        "prompt-queue.empty",
                        "queued prompt text cannot be empty",
                        false,
                    ));
                }
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
            CommandBody::PromoteQueuedPrompt { session_id, .. }
            | CommandBody::DeleteQueuedPrompt { session_id, .. }
                if !session_exists(pool, *session_id)
                    .await
                    .map_err(internal_error)? =>
            {
                return Err(CodypendentError::new(
                    "protocol.session-not-found",
                    format!("no session {session_id}"),
                    false,
                ));
            }
            _ => {}
        }
        Ok(())
    }

    // --- per-command handlers -------------------------------------------------

    // The argument list mirrors the row this inserts (workspace, title,
    // repository, and the three council-parentage columns from
    // migrations/0040_session_library.sql); grouping them into a struct would
    // just move the same fields behind another name.
    #[allow(clippy::too_many_arguments)]
    async fn apply_create_session(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        workspace_id: WorkspaceId,
        title: String,
        repository: Option<String>,
        internal: bool,
        parent_session_id: Option<SessionId>,
        parent_run_id: Option<RunId>,
    ) -> Result<CommandOutcome, CodypendentError> {
        let session_id = SessionId::new();
        let repository_id = repository
            .as_deref()
            .map(Path::new)
            .map(codypendent_knowledge::stable_repository_id);
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
                owner_uid: ctx.principal.uid(),
                workspace_id,
                repository_id,
                repository: repository.as_deref(),
                internal,
                parent_session_id,
                parent_run_id,
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

    async fn apply_session_lifecycle(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        session_id: SessionId,
        action: SessionLifecycleAction,
    ) -> Result<CommandOutcome, CodypendentError> {
        let rename = match &action {
            SessionLifecycleAction::Rename { title } if title.trim().is_empty() => {
                return Err(CodypendentError::new(
                    "session-library.invalid-title",
                    "session title cannot be empty",
                    false,
                ));
            }
            SessionLifecycleAction::Rename { title } => Some(title.trim().to_owned()),
            SessionLifecycleAction::Unknown => {
                return Err(reserved_unsupported_error());
            }
            _ => None,
        };
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let mut tx = pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(internal_error)?;
        let (revision,): (i64,) =
            sqlx::query_as("SELECT revision FROM sessions WHERE id = ? AND tombstoned_at IS NULL")
                .bind(session_id.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(internal_error)?
                .ok_or_else(|| {
                    CodypendentError::new(
                        "protocol.session-not-found",
                        format!("no session {session_id}"),
                        false,
                    )
                })?;
        if let Some(expected) = command.expected_revision {
            let current = u64::try_from(revision).map_err(internal_error)?;
            if current != expected {
                return Err(CodypendentError::new(
                    "protocol.revision-mismatch",
                    format!("expected revision {expected}, session is at {current}"),
                    false,
                ));
            }
        }

        // NOTE: an earlier draft called `PolicyEngine::check_static_action` here
        // with an `ApprovalAction::Session*` action set. Neither exists:
        // `crate::policy::ApprovalAction` is the disposition enum
        // (Allow/Approval/AlwaysApproval/Deny) and `PolicyEngine` exposes
        // `evaluate(&ProposedAction, &EvalContext)`, which has no session
        // lifecycle variant. Session lifecycle authorization is enforced by
        // `role_permits` + `named_resources` ownership gating below.

        let mut exported_artifact = None;
        let inserted = sqlx::query("INSERT INTO commands (id, idempotency_key, session_id, client_id, body, status, received_at) VALUES (?, ?, ?, ?, ?, 'received', ?)")
            .bind(command.command_id.to_string()).bind(&command.idempotency_key)
            .bind(session_id.to_string()).bind(ctx.client_id.to_string())
            .bind(serde_json::to_string(&command.body).map_err(internal_error)?).bind(&now_str)
            .execute(&mut *tx).await;
        if let Err(error) = inserted {
            let _ = tx.rollback().await;
            let error = anyhow::Error::from(error);
            if is_unique_violation(&error) {
                if let Some(existing) = lookup_command(pool, &command.idempotency_key)
                    .await
                    .map_err(internal_error)?
                {
                    reject_replay_mismatch(&command.body, &existing.body)?;
                    return self.handle_existing(pool, existing).await;
                }
            }
            return Err(internal_error(error));
        }

        let note = match &action {
            SessionLifecycleAction::Rename { .. } => {
                sqlx::query("UPDATE sessions SET title = ? WHERE id = ?")
                    .bind(rename.as_deref().unwrap())
                    .bind(session_id.to_string())
                    .execute(&mut *tx)
                    .await
                    .map_err(internal_error)?;
                crate::session_library::index_title_source(
                    &mut *tx,
                    session_id,
                    rename.as_deref().unwrap(),
                    &now_str,
                )
                .await
                .map_err(internal_error)?;
                "session renamed"
            }
            SessionLifecycleAction::Pin | SessionLifecycleAction::Unpin => {
                let pinned = i64::from(matches!(action, SessionLifecycleAction::Pin));
                sqlx::query("UPDATE sessions SET pinned = ? WHERE id = ?")
                    .bind(pinned)
                    .bind(session_id.to_string())
                    .execute(&mut *tx)
                    .await
                    .map_err(internal_error)?;
                if pinned == 1 {
                    "session pinned"
                } else {
                    "session unpinned"
                }
            }
            SessionLifecycleAction::Archive => {
                sqlx::query(
                    "UPDATE sessions SET archived_at = COALESCE(archived_at, ?) WHERE id = ?",
                )
                .bind(&now_str)
                .bind(session_id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(internal_error)?;
                "session archived"
            }
            SessionLifecycleAction::Restore => {
                sqlx::query("UPDATE sessions SET archived_at = NULL WHERE id = ?")
                    .bind(session_id.to_string())
                    .execute(&mut *tx)
                    .await
                    .map_err(internal_error)?;
                "session restored"
            }
            SessionLifecycleAction::Delete { mode } => {
                let (mode, purge_after) = match mode {
                    SessionDeletionMode::RetentionPolicy => (
                        "retention_policy",
                        Some((now + chrono::Duration::days(30)).to_rfc3339()),
                    ),
                    SessionDeletionMode::TombstoneOnly => ("tombstone_only", None),
                    _ => {
                        return Err(CodypendentError::new(
                            "session-library.invalid-deletion-mode",
                            "unknown session deletion mode",
                            false,
                        ))
                    }
                };
                sqlx::query("UPDATE sessions SET tombstoned_at = ?, deletion_mode = ?, purge_after = ? WHERE id = ?")
                    .bind(&now_str).bind(mode).bind(purge_after).bind(session_id.to_string())
                    .execute(&mut *tx).await.map_err(internal_error)?;
                "session tombstoned"
            }
            SessionLifecycleAction::Export { options } => {
                let artifact = crate::bundles::export_session_lifecycle(
                    pool,
                    &self.artifacts_or_default(),
                    ctx.principal.uid(),
                    session_id,
                    options,
                )
                .await?;
                exported_artifact = Some(artifact);
                "session exported"
            }
            // Fail closed: `SessionLifecycleAction` is `#[non_exhaustive]`, so a
            // variant this daemon does not implement (including anything a newer
            // client adds) is refused rather than silently applied as a no-op
            // that still bumps the revision and appends a note.
            SessionLifecycleAction::Unknown => return Err(reserved_unsupported_error()),
            _ => return Err(reserved_unsupported_error()),
        };
        let sequence = next_sequence(&mut *tx, session_id)
            .await
            .map_err(internal_error)?;
        let actor = Actor::Client {
            client_id: ctx.client_id,
        };
        let body = EventBody::NoteAppended {
            text: note.into(),
            run_id: None,
        };
        append_event(
            &mut tx,
            session_id,
            sequence,
            &actor,
            &body,
            &now_str,
            Some(command.command_id),
        )
        .await
        .map_err(internal_error)?;
        sqlx::query("UPDATE sessions SET revision = revision + 1, updated_at = ?, last_activity_at = ? WHERE id = ?")
            .bind(&now_str).bind(&now_str).bind(session_id.to_string()).execute(&mut *tx).await.map_err(internal_error)?;
        let lifecycle_response = if matches!(action, SessionLifecycleAction::Delete { .. }) {
            LifecycleResponse::Deleted {
                session_id,
                tombstoned: true,
            }
        } else if let Some(artifact) = exported_artifact {
            LifecycleResponse::Exported { artifact }
        } else {
            LifecycleResponse::Summary(Box::new(
                load_lifecycle_summary_in(&mut *tx, session_id).await?,
            ))
        };
        let outcome = CommandOutcome {
            command_id: command.command_id,
            created_session: None,
            created_run: None,
            last_sequence: Some(u64::try_from(sequence).map_err(internal_error)?),
            newly_applied: true,
        };
        sqlx::query(
            "UPDATE commands SET status = 'applied', result_json = ?, applied_at = ? WHERE id = ?",
        )
        .bind(
            serde_json::to_string(&PersistedLifecycleOutcome {
                outcome: outcome.clone(),
                lifecycle_response,
            })
            .map_err(internal_error)?,
        )
        .bind(&now_str)
        .bind(command.command_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;
        crate::control_plane_sync::outbox::enqueue_session_snapshot(
            &mut tx,
            &session_id.to_string(),
        )
        .await
        .map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;
        self.subscriptions.publish(
            session_id,
            SessionEvent {
                sequence: outcome.last_sequence.unwrap(),
                occurred_at: now,
                causation_id: Some(command.command_id),
                correlation_id: None,
                actor,
                body,
            },
        );
        Ok(outcome)
    }

    /// Atomically close a session and append its sole closure event. Once the
    /// session is closed, later keys are recorded as accepted no-ops.
    async fn apply_close_session(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        session_id: SessionId,
    ) -> Result<CommandOutcome, CodypendentError> {
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let mut tx = pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(internal_error)?;
        let (state, revision): (String, i64) =
            sqlx::query_as("SELECT state, revision FROM sessions WHERE id = ?")
                .bind(session_id.to_string())
                .fetch_one(&mut *tx)
                .await
                .map_err(internal_error)?;
        let revision = u64::try_from(revision).map_err(internal_error)?;

        // `expected_revision` guards the state transition only. An already
        // closed session is an unconditional semantic no-op, so retries under a
        // different key cannot turn successful closure into a stale conflict.
        if state != "closed" {
            if let Some(expected) = command.expected_revision {
                if expected != revision {
                    return Err(revision_conflict(expected, revision));
                }
            }

            // Closing must not make the session terminal while work that still
            // needs to append lifecycle events or finish an external effect is
            // live. Check under the same write lock as the transition, before
            // reserving this CloseSession command, so rejection leaves no trace.
            let run_states: Vec<(String,)> =
                sqlx::query_as("SELECT state FROM runs WHERE session_id = ?")
                    .bind(session_id.to_string())
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(internal_error)?;
            if run_states
                .iter()
                .any(|(state,)| !projections::is_terminal(projections::run_state_from_db(state)))
            {
                return Err(session_has_active_run());
            }

            // A terminal projection is written immediately before the richer,
            // authoritative completion event. Do not close in that ordering
            // window: closure would make append_next_event reject RunCompleted
            // and permanently lose the run's disposition and chronicle. JSON
            // extraction is intentional here; event identity and run identity
            // must be exact, not inferred from a substring of serialized data.
            let completion_evidence: Vec<(String, Option<String>)> = sqlx::query_as(
                "SELECT r.id, (\
                     SELECT e.body FROM events e \
                     WHERE e.session_id = r.session_id \
                       AND CASE WHEN json_valid(e.body) \
                           THEN json_extract(e.body, '$.type') END = 'RunCompleted' \
                       AND CASE WHEN json_valid(e.body) \
                           THEN json_extract(e.body, '$.run_id') END = r.id \
                     ORDER BY e.sequence DESC LIMIT 1\
                 ) \
                 FROM runs r WHERE r.session_id = ?",
            )
            .bind(session_id.to_string())
            .fetch_all(&mut *tx)
            .await
            .map_err(internal_error)?;
            for (expected_run, evidence) in completion_evidence {
                let Some(evidence) = evidence else {
                    return Err(session_run_evidence_pending());
                };
                let parsed: EventBody =
                    serde_json::from_str(&evidence).map_err(|_| session_run_evidence_pending())?;
                let expected_run = RunId::from_str(&expected_run).map_err(internal_error)?;
                match parsed {
                    EventBody::RunCompleted {
                        run_id,
                        disposition:
                            codypendent_protocol::RunDisposition::Completed { .. }
                            | codypendent_protocol::RunDisposition::Failed { .. }
                            | codypendent_protocol::RunDisposition::Cancelled { .. },
                        ..
                    } if run_id == expected_run => {}
                    _ => return Err(session_run_evidence_pending()),
                }
            }

            // Pending approval/question rows have daemon housekeeping paths
            // that resolve them and append audit events. Keep the session open
            // until that work is settled; otherwise a later expiry could append
            // after SessionClosed through those direct transactional paths.
            let (pending_human_work,): (i64,) = sqlx::query_as(
                "SELECT \
                     (SELECT COUNT(*) FROM approvals a JOIN runs r ON r.id = a.run_id \
                      WHERE r.session_id = ? AND a.state = 'pending') + \
                     (SELECT COUNT(*) FROM questions q JOIN runs r ON r.id = q.run_id \
                      WHERE r.session_id = ? AND q.state = 'pending')",
            )
            .bind(session_id.to_string())
            .bind(session_id.to_string())
            .fetch_one(&mut *tx)
            .await
            .map_err(internal_error)?;
            if pending_human_work != 0 {
                return Err(session_has_pending_human_work());
            }

            // Checkpoint restore approvals wake an out-of-band continuation.
            // After the approval row resolves it is no longer counted above,
            // but closure must still wait for the matching audit event: the
            // continuation performs a worktree effect before appending it.
            let (pending_restore_effects,): (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM approvals a \
                 JOIN runs r ON r.id = a.run_id \
                 JOIN run_checkpoints cp ON cp.run_id = r.id \
                    AND cp.ordinal = json_extract(a.action_json, '$.ordinal') \
                 WHERE r.session_id = ? \
                   AND a.state IN ('approved', 'rejected', 'expired') \
                   AND json_valid(a.action_json) \
                   AND json_extract(a.action_json, '$.type') = 'RestoreCheckpoint' \
                   AND NOT EXISTS (\
                       SELECT 1 FROM events e \
                       WHERE e.session_id = r.session_id \
                         AND json_valid(e.body) \
                         AND json_extract(e.body, '$.type') = 'CheckpointRestored' \
                         AND json_extract(e.body, '$.checkpoint_id') = cp.id\
                   )",
            )
            .bind(session_id.to_string())
            .fetch_one(&mut *tx)
            .await
            .map_err(internal_error)?;
            if pending_restore_effects != 0 {
                return Err(session_has_effect_in_flight());
            }

            let (received_commands,): (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM commands WHERE session_id = ? AND status = 'received'",
            )
            .bind(session_id.to_string())
            .fetch_one(&mut *tx)
            .await
            .map_err(internal_error)?;
            if received_commands != 0 {
                return Err(session_has_received_command());
            }
        }

        let insert = sqlx::query(
            "INSERT INTO commands (id, idempotency_key, session_id, client_id, body, status, received_at) \
             VALUES (?, ?, ?, ?, ?, 'received', ?)",
        )
        .bind(command.command_id.to_string())
        .bind(&command.idempotency_key)
        .bind(session_id.to_string())
        .bind(ctx.client_id.to_string())
        .bind(serde_json::to_string(&command.body).map_err(internal_error)?)
        .bind(&now_str)
        .execute(&mut *tx)
        .await;
        if let Err(error) = insert {
            drop(tx);
            let error = anyhow::Error::new(error);
            if is_unique_violation(&error) {
                if let Some(existing) = lookup_command(pool, &command.idempotency_key)
                    .await
                    .map_err(internal_error)?
                {
                    reject_replay_mismatch(&command.body, &existing.body)?;
                    return self.handle_existing(pool, existing).await;
                }
            }
            return Err(internal_error(error));
        }

        let mut persisted = None;
        if state != "closed" {
            sqlx::query(
                "UPDATE sessions SET state = 'closed', revision = revision + 1, updated_at = ? WHERE id = ?",
            )
            .bind(&now_str)
            .bind(session_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;
            let sequence = next_sequence(&mut *tx, session_id)
                .await
                .map_err(internal_error)?;
            let actor = Actor::Client {
                client_id: ctx.client_id,
            };
            append_event(
                &mut tx,
                session_id,
                sequence,
                &actor,
                &EventBody::SessionClosed,
                &now_str,
                Some(command.command_id),
            )
            .await
            .map_err(internal_error)?;
            persisted = Some(SessionEvent {
                sequence: u64::try_from(sequence).map_err(internal_error)?,
                occurred_at: now,
                causation_id: Some(command.command_id),
                correlation_id: None,
                actor,
                body: EventBody::SessionClosed,
            });
        }
        let outcome = CommandOutcome {
            command_id: command.command_id,
            created_session: None,
            created_run: None,
            last_sequence: persisted.as_ref().map(|event| event.sequence),
            newly_applied: true,
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
        crate::control_plane_sync::outbox::enqueue_session_snapshot(
            &mut tx,
            &session_id.to_string(),
        )
        .await
        .map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;
        if let Some(event) = persisted {
            self.subscriptions.publish(session_id, event);
        }
        Ok(outcome)
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

    /// A follow-up message CONTINUES the conversation: it launches a new bounded
    /// run whose objective is the user's `text` (continuous-session plan, Task 3).
    /// Mirrors [`apply_start_run`](Self::apply_start_run) exactly — mint a
    /// `RunId`, append `RunStarted`, insert the run projection, under the same
    /// role gate and `expected_revision`/idempotency handling — so a
    /// `SubmitUserInput` becomes an ordinary run. The daemon does NOT reconstruct
    /// the prior transcript here (it cannot build the runtime's `TurnItem`); the
    /// assembly executor seeds that from the session ledger at run start.
    async fn apply_submit_input(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        session_id: SessionId,
        text: String,
        mode: AgentMode,
    ) -> Result<CommandOutcome, CodypendentError> {
        let run_id = RunId::new();
        let events = vec![(
            Actor::Client {
                client_id: ctx.client_id,
            },
            EventBody::RunStarted {
                run_id,
                objective: text.clone(),
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
                objective: text,
                mode,
            },
            (None, Some(run_id)),
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

        let text = match &command.body {
            CommandBody::QueueSteering { text, .. } => text.clone(),
            _ => String::new(),
        };

        let run_mode = projections::load_run_mode(pool, run_id)
            .await
            .map_err(internal_error)?
            .unwrap_or(AgentMode::Build);

        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let body_json = serde_json::to_string(&command.body).map_err(internal_error)?;

        let mut tx = begin_session_write(pool, session_id).await?;

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

        let prompts = if !text.trim().is_empty() {
            crate::prompt_queue::enqueue(
                &mut tx,
                session_id,
                &text,
                run_mode,
                PromptDelivery::Steer,
            )
            .await
            .map_err(internal_error)?
        } else {
            crate::prompt_queue::snapshot(&mut tx, session_id)
                .await
                .map_err(internal_error)?
        };

        let seq1 = next_sequence(&mut *tx, session_id)
            .await
            .map_err(internal_error)?;
        let actor = Actor::Client {
            client_id: ctx.client_id,
        };
        let body1 = EventBody::SteeringQueued { run_id };
        append_event(
            &mut tx,
            session_id,
            seq1,
            &actor,
            &body1,
            &now_str,
            Some(command.command_id),
        )
        .await
        .map_err(internal_error)?;

        let seq2 = next_sequence(&mut *tx, session_id)
            .await
            .map_err(internal_error)?;
        let body2 = EventBody::PendingPromptsChanged { prompts };
        append_event(
            &mut tx,
            session_id,
            seq2,
            &actor,
            &body2,
            &now_str,
            Some(command.command_id),
        )
        .await
        .map_err(internal_error)?;

        sqlx::query("UPDATE sessions SET revision = revision + 1, updated_at = ? WHERE id = ?")
            .bind(&now_str)
            .bind(session_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;

        sqlx::query("UPDATE commands SET status = 'applied', applied_at = ? WHERE id = ?")
            .bind(&now_str)
            .bind(command.command_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;

        crate::control_plane_sync::outbox::enqueue_session_snapshot(
            &mut tx,
            &session_id.to_string(),
        )
        .await
        .map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;

        let event1 = SessionEvent {
            sequence: u64::try_from(seq1).map_err(internal_error)?,
            occurred_at: now,
            causation_id: Some(command.command_id),
            correlation_id: None,
            actor: actor.clone(),
            body: body1,
        };
        let event2 = SessionEvent {
            sequence: u64::try_from(seq2).map_err(internal_error)?,
            occurred_at: now,
            causation_id: Some(command.command_id),
            correlation_id: None,
            actor,
            body: body2,
        };
        self.subscriptions.publish(session_id, event1.clone());
        self.subscriptions.publish(session_id, event2.clone());

        Ok(CommandOutcome {
            command_id: command.command_id,
            created_session: None,
            created_run: None,
            last_sequence: Some(u64::try_from(seq2).map_err(internal_error)?),
            newly_applied: true,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_queue_prompt(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        session_id: SessionId,
        text: String,
        mode: AgentMode,
        delivery: PromptDelivery,
    ) -> Result<CommandOutcome, CodypendentError> {
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let body_json = serde_json::to_string(&command.body).map_err(internal_error)?;

        let mut tx = begin_session_write(pool, session_id).await?;

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

        let prompts = crate::prompt_queue::enqueue(&mut tx, session_id, &text, mode, delivery)
            .await
            .map_err(internal_error)?;

        let seq = next_sequence(&mut *tx, session_id)
            .await
            .map_err(internal_error)?;
        let actor = Actor::Client {
            client_id: ctx.client_id,
        };
        let body = EventBody::PendingPromptsChanged { prompts };

        append_event(
            &mut tx,
            session_id,
            seq,
            &actor,
            &body,
            &now_str,
            Some(command.command_id),
        )
        .await
        .map_err(internal_error)?;

        sqlx::query("UPDATE sessions SET revision = revision + 1, updated_at = ? WHERE id = ?")
            .bind(&now_str)
            .bind(session_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;

        let outcome = CommandOutcome {
            command_id: command.command_id,
            created_session: None,
            created_run: None,
            last_sequence: Some(u64::try_from(seq).map_err(internal_error)?),
            newly_applied: true,
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

        crate::control_plane_sync::outbox::enqueue_session_snapshot(
            &mut tx,
            &session_id.to_string(),
        )
        .await
        .map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;

        let event = SessionEvent {
            sequence: u64::try_from(seq).map_err(internal_error)?,
            occurred_at: now,
            causation_id: Some(command.command_id),
            correlation_id: None,
            actor,
            body,
        };
        self.subscriptions.publish(session_id, event.clone());

        Ok(outcome)
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_update_queued_prompt(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        session_id: SessionId,
        prompt_id: PromptId,
        text: Option<String>,
        delivery: Option<PromptDelivery>,
    ) -> Result<CommandOutcome, CodypendentError> {
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let body_json = serde_json::to_string(&command.body).map_err(internal_error)?;

        let mut tx = begin_session_write(pool, session_id).await?;

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

        let updated =
            crate::prompt_queue::update(&mut tx, session_id, prompt_id, text.as_deref(), delivery)
                .await
                .map_err(internal_error)?;

        let prompts = match updated {
            Some(p) => p,
            None => {
                let _ = tx.rollback().await;
                return Err(CodypendentError::new(
                    "prompt-queue.not-found",
                    format!("no queued prompt {prompt_id}"),
                    false,
                ));
            }
        };

        let seq = next_sequence(&mut *tx, session_id)
            .await
            .map_err(internal_error)?;
        let actor = Actor::Client {
            client_id: ctx.client_id,
        };
        let body = EventBody::PendingPromptsChanged { prompts };

        append_event(
            &mut tx,
            session_id,
            seq,
            &actor,
            &body,
            &now_str,
            Some(command.command_id),
        )
        .await
        .map_err(internal_error)?;

        sqlx::query("UPDATE sessions SET revision = revision + 1, updated_at = ? WHERE id = ?")
            .bind(&now_str)
            .bind(session_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;

        let outcome = CommandOutcome {
            command_id: command.command_id,
            created_session: None,
            created_run: None,
            last_sequence: Some(u64::try_from(seq).map_err(internal_error)?),
            newly_applied: true,
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

        crate::control_plane_sync::outbox::enqueue_session_snapshot(
            &mut tx,
            &session_id.to_string(),
        )
        .await
        .map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;

        let event = SessionEvent {
            sequence: u64::try_from(seq).map_err(internal_error)?,
            occurred_at: now,
            causation_id: Some(command.command_id),
            correlation_id: None,
            actor,
            body,
        };
        self.subscriptions.publish(session_id, event.clone());

        Ok(outcome)
    }

    async fn apply_promote_queued_prompt(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        session_id: SessionId,
        prompt_id: PromptId,
    ) -> Result<CommandOutcome, CodypendentError> {
        self.apply_update_queued_prompt(
            pool,
            ctx,
            command,
            session_id,
            prompt_id,
            None,
            Some(PromptDelivery::Steer),
        )
        .await
    }

    async fn apply_delete_queued_prompt(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        session_id: SessionId,
        prompt_id: PromptId,
    ) -> Result<CommandOutcome, CodypendentError> {
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let body_json = serde_json::to_string(&command.body).map_err(internal_error)?;

        let mut tx = begin_session_write(pool, session_id).await?;

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

        let deleted = crate::prompt_queue::delete(&mut tx, session_id, prompt_id)
            .await
            .map_err(internal_error)?;

        let prompts = match deleted {
            Some(p) => p,
            None => {
                let _ = tx.rollback().await;
                return Err(CodypendentError::new(
                    "prompt-queue.not-found",
                    format!("no queued prompt {prompt_id}"),
                    false,
                ));
            }
        };

        let seq = next_sequence(&mut *tx, session_id)
            .await
            .map_err(internal_error)?;
        let actor = Actor::Client {
            client_id: ctx.client_id,
        };
        let body = EventBody::PendingPromptsChanged { prompts };

        append_event(
            &mut tx,
            session_id,
            seq,
            &actor,
            &body,
            &now_str,
            Some(command.command_id),
        )
        .await
        .map_err(internal_error)?;

        sqlx::query("UPDATE sessions SET revision = revision + 1, updated_at = ? WHERE id = ?")
            .bind(&now_str)
            .bind(session_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;

        let outcome = CommandOutcome {
            command_id: command.command_id,
            created_session: None,
            created_run: None,
            last_sequence: Some(u64::try_from(seq).map_err(internal_error)?),
            newly_applied: true,
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

        crate::control_plane_sync::outbox::enqueue_session_snapshot(
            &mut tx,
            &session_id.to_string(),
        )
        .await
        .map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;

        let event = SessionEvent {
            sequence: u64::try_from(seq).map_err(internal_error)?,
            occurred_at: now,
            causation_id: Some(command.command_id),
            correlation_id: None,
            actor,
            body,
        };
        self.subscriptions.publish(session_id, event.clone());

        Ok(outcome)
    }

    /// Apply a `!` operator shell escape. This is an operator-initiated shell
    /// escape that runs CONFINED under the session's sandbox profile in the
    /// session worktree: `/bin/sh -c <cmd>` still supports pipes and redirects,
    /// but it executes inside the platform sandbox (Seatbelt on macOS / bwrap on
    /// Linux) scoped to the worktree (read+write there, no network), bounded
    /// (memory/CPU/output) and time-limited (wall-clock kill). It FAILS CLOSED:
    /// when the sandbox cannot be enforced (unsupported platform, missing tool)
    /// the command is REFUSED with a legible note, never run unconfined.
    ///
    /// Crash-consistency: a `!` command is a NON-idempotent external effect, so
    /// it must never re-execute on resume. The `received` row is committed in
    /// its OWN transaction BEFORE the subprocess runs and the subprocess runs
    /// OUTSIDE any write transaction — so a crash mid-exec leaves a durable
    /// `received` row that [`resume_received`] finalizes applied-without-re-exec
    /// (the `RunUserShell` body falls through its idempotent-effect arms), never
    /// a second run. The write lock is NOT held across the subprocess.
    async fn apply_run_user_shell(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        session_id: SessionId,
        shell_cmd: String,
    ) -> Result<CommandOutcome, CodypendentError> {
        let body_json = serde_json::to_string(&command.body).map_err(internal_error)?;

        // --- Claim: commit the `received` row in its OWN transaction, before
        // the subprocess runs, so a mid-exec crash leaves a durable claim the
        // resume path finalizes without re-executing.
        {
            let mut tx = begin_session_write(pool, session_id).await?;
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
            .bind(Utc::now().to_rfc3339())
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
            tx.commit().await.map_err(internal_error)?;
        }

        // --- Run OUTSIDE any write transaction, in the session's repository.
        // The session's originating `StartRun` is authoritative for the repo.
        // A database error is NOT equivalent to "this old session had no
        // repository": executing against the daemon cwd in that case can run a
        // shell command in a different checkout. Fail closed, but still drive
        // the already-committed `received` claim through the normal apply phase
        // below so replay never executes the non-idempotent command later.
        let output_text = match session_run_provenance(pool, session_id).await {
            Ok(provenance) => {
                let cwd = resolve_run_repository(provenance.repository.as_deref());
                // Canonicalize so the confined cwd matches the granted worktree
                // path exactly (macOS tempdirs are symlinks under /var →
                // /private/var).
                let cwd = cwd.canonicalize().unwrap_or(cwd);
                let origin = format!("shell:{session_id}");
                run_user_shell_command(&shell_cmd, &cwd, &origin).await
            }
            Err(error) => {
                tracing::error!(
                    %session_id,
                    %error,
                    "refusing user shell command because session provenance could not be loaded"
                );
                "Shell command was not run because the session repository could not be resolved safely."
                    .to_string()
            }
        };

        // --- Apply: append the two notes + mark applied in a second tx. The
        // write lock is taken only now, never across the subprocess above.
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let mut tx = begin_session_write(pool, session_id).await?;

        // 1. User note: $ <cmd>
        let seq1 = next_sequence(&mut *tx, session_id)
            .await
            .map_err(internal_error)?;
        let actor1 = Actor::Client {
            client_id: ctx.client_id,
        };
        let body1 = EventBody::NoteAppended {
            text: format!("$ {shell_cmd}"),
            run_id: None,
        };
        append_event(
            &mut tx,
            session_id,
            seq1,
            &actor1,
            &body1,
            &now_str,
            Some(command.command_id),
        )
        .await
        .map_err(internal_error)?;

        // 2. Output note
        let seq2 = next_sequence(&mut *tx, session_id)
            .await
            .map_err(internal_error)?;
        let actor2 = Actor::System;
        let body2 = EventBody::NoteAppended {
            text: output_text,
            run_id: None,
        };
        append_event(
            &mut tx,
            session_id,
            seq2,
            &actor2,
            &body2,
            &now_str,
            Some(command.command_id),
        )
        .await
        .map_err(internal_error)?;

        sqlx::query("UPDATE sessions SET revision = revision + 1, updated_at = ? WHERE id = ?")
            .bind(&now_str)
            .bind(session_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;

        let outcome = CommandOutcome {
            command_id: command.command_id,
            created_session: None,
            created_run: None,
            last_sequence: Some(u64::try_from(seq2).map_err(internal_error)?),
            newly_applied: true,
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

        crate::control_plane_sync::outbox::enqueue_session_snapshot(
            &mut tx,
            &session_id.to_string(),
        )
        .await
        .map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;

        let event1 = SessionEvent {
            sequence: u64::try_from(seq1).map_err(internal_error)?,
            occurred_at: now,
            causation_id: Some(command.command_id),
            correlation_id: None,
            actor: actor1,
            body: body1,
        };
        self.subscriptions.publish(session_id, event1);

        let event2 = SessionEvent {
            sequence: u64::try_from(seq2).map_err(internal_error)?,
            occurred_at: now,
            causation_id: Some(command.command_id),
            correlation_id: None,
            actor: actor2,
            body: body2,
        };
        self.subscriptions.publish(session_id, event2);

        Ok(outcome)
    }

    /// The composer's `#` quick-add (Spec 20 Action 20).
    ///
    /// The protocol documents this as "gated by the curator's secret and dedup
    /// filters", and it now is: the text is run through the SAME
    /// [`detect_secret`](codypendent_knowledge::detect_secret) filter
    /// `MemoryStore::curate` opens with, and — when it survives — through
    /// `curate` itself, so a memory is genuinely stored and the dedup,
    /// contradiction, provenance and retention gates genuinely run. Appending a
    /// note was never any of that.
    ///
    /// **Order matters.** The secret filter runs BEFORE anything is written,
    /// because the ledger is append-only: a pasted key reaching a `NoteAppended`
    /// event could never be taken back, and `curate`'s own redaction gate fires
    /// too late to help (the note would already be durable). A refused quick-add
    /// records that it was refused and why — never the text that was refused.
    ///
    /// That covers BOTH durable writes this handler makes. The `commands` row it
    /// inserts for idempotency stores the command body, and the body carries the
    /// raw `text`; a refused quick-add therefore persists a redacted body, so
    /// the secret lands in neither `events` nor `commands`.
    ///
    /// The curation itself runs AFTER the command transaction commits, exactly
    /// as the executor's post-run harvest does: `curate` opens its own
    /// transaction, and nesting that inside this one's `BEGIN IMMEDIATE` would
    /// deadlock on the write lock. A curation failure is logged, never fatal —
    /// the note is already durable and the operator has their receipt.
    ///
    /// The memory is anchored at the operator's local user scope, so it
    /// resurfaces in later runs (`emit_context` queries System + local user +
    /// repository); a session-scoped memory would never be seen again. That
    /// scope is reserved for `Preference`-class facts, which is exactly what an
    /// operator typing "remember this" is asserting.
    async fn apply_remember_memory(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        session_id: SessionId,
        text: String,
    ) -> Result<CommandOutcome, CodypendentError> {
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        // Gate (a) of the curator pipeline, hoisted ahead of EVERY write.
        let secret = codypendent_knowledge::detect_secret(&text);

        // The `commands` row is durable too. Serializing `command.body` verbatim
        // would have parked the very text the ledger refused in the `body`
        // column — same database, same backups, same exports — so the persisted
        // body carries a redacted `text` whenever the filter fires. The row is
        // still a faithful record of WHICH command arrived and still keys
        // idempotency; only the payload is scrubbed. Replay is unaffected:
        // `handle_existing` returns the recorded `result_json` for an `applied`
        // row and `resume_received` re-drives only `ForkSession` /
        // `ResolveApproval` / `ResolveQuestion` external effects, so a
        // `RememberMemory` body is never re-executed from disk — a redacted row
        // resolves to the same refusal outcome, never to a different action.
        let persisted_body = match &secret {
            Some(reason) => CommandBody::RememberMemory {
                session_id,
                text: format!("[redacted: refused by the secret filter ({reason})]"),
            },
            None => command.body.clone(),
        };
        let body_json = serde_json::to_string(&persisted_body).map_err(internal_error)?;

        let mut tx = begin_session_write(pool, session_id).await?;

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

        let seq = next_sequence(&mut *tx, session_id)
            .await
            .map_err(internal_error)?;
        let actor = Actor::System;
        let body = EventBody::NoteAppended {
            text: match &secret {
                // Never echo the refused text — not even truncated.
                Some(reason) => {
                    format!("memory not saved: refused by the secret filter ({reason})")
                }
                None => format!("remembered: {text}"),
            },
            run_id: None,
        };

        append_event(
            &mut tx,
            session_id,
            seq,
            &actor,
            &body,
            &now_str,
            Some(command.command_id),
        )
        .await
        .map_err(internal_error)?;

        sqlx::query("UPDATE sessions SET revision = revision + 1, updated_at = ? WHERE id = ?")
            .bind(&now_str)
            .bind(session_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;

        let outcome = CommandOutcome {
            command_id: command.command_id,
            created_session: None,
            created_run: None,
            last_sequence: Some(u64::try_from(seq).map_err(internal_error)?),
            newly_applied: true,
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

        crate::control_plane_sync::outbox::enqueue_session_snapshot(
            &mut tx,
            &session_id.to_string(),
        )
        .await
        .map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;

        let event = SessionEvent {
            sequence: u64::try_from(seq).map_err(internal_error)?,
            occurred_at: now,
            causation_id: Some(command.command_id),
            correlation_id: None,
            actor,
            body,
        };
        self.subscriptions.publish(session_id, event);

        if secret.is_none() {
            curate_quick_add_memory(pool, session_id, seq, &text, now).await;
        }

        Ok(outcome)
    }

    async fn apply_run_editor_action(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        session_id: SessionId,
        action: codypendent_protocol::EditorNativeAction,
        // GAP (not fixable in this file): the editor's active file, selection and
        // diagnostics arrive here and are dropped. The run launcher in
        // `server.rs` destructures `RunEditorAction { session_id, action, model,
        // .. }` and drops it too, so nothing downstream sees the IDE context the
        // client took the trouble to send. Kept in the signature so the seam is
        // visible where it needs to be threaded.
        _context: codypendent_protocol::EditorActionContext,
    ) -> Result<CommandOutcome, CodypendentError> {
        let objective = match action {
            codypendent_protocol::EditorNativeAction::FixSelection => "Fix selection".to_string(),
            codypendent_protocol::EditorNativeAction::ExplainSelection => {
                "Explain selection".to_string()
            }
            codypendent_protocol::EditorNativeAction::ReviewCurrentFile => {
                "Review current file".to_string()
            }
            codypendent_protocol::EditorNativeAction::GenerateTestsForSelection => {
                "Generate tests for selection".to_string()
            }
            codypendent_protocol::EditorNativeAction::FixDiagnostic { diagnostic } => {
                format!("Fix diagnostic: {}", diagnostic.message)
            }
            // Fail closed: `EditorNativeAction` is `#[non_exhaustive]` and its
            // `Unknown` arm is the `#[serde(other)]` catch, so an action this
            // daemon does not implement must be refused — not started as a run
            // with a generic "Run editor action" objective the model would then
            // have to guess at.
            codypendent_protocol::EditorNativeAction::Unknown => {
                return Err(reserved_unsupported_error())
            }
            _ => return Err(reserved_unsupported_error()),
        };
        let run_id = RunId::new();
        let events = vec![(
            Actor::Client {
                client_id: ctx.client_id,
            },
            EventBody::RunStarted {
                run_id,
                objective: objective.clone(),
                mode: AgentMode::Build,
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
                mode: AgentMode::Build,
            },
            (None, Some(run_id)),
            RevisionOp::Bump {
                expected: command.expected_revision,
            },
        )
        .await
    }

    async fn apply_export_bundle(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        request: &codypendent_protocol::bundle::BundleExportRequest,
    ) -> Result<CommandOutcome, CodypendentError> {
        let _receipt = crate::bundles::export(
            pool,
            &self.artifacts_or_default(),
            ctx.principal.uid(),
            ctx.client_id,
            command.command_id,
            &command.idempotency_key,
            request,
        )
        .await?;
        Ok(CommandOutcome {
            command_id: command.command_id,
            created_session: None,
            created_run: None,
            last_sequence: None,
            newly_applied: true,
        })
    }

    async fn apply_export_analytics(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        request: &codypendent_protocol::AnalyticsExportRequest,
    ) -> Result<CommandOutcome, CodypendentError> {
        let result = crate::analytics::export::export(
            pool,
            &self.artifacts_or_default(),
            ctx.principal.uid(),
            ctx.principal,
            ctx.client_id,
            command.command_id,
            request,
        )
        .await
        .map_err(|e| match e {
            crate::analytics::AnalyticsError::UnsupportedFormat => CodypendentError::new(
                "analytics.unsupported-format",
                "the requested analytics export format is unsupported",
                false,
            ),
            crate::analytics::AnalyticsError::UnsupportedGrouping => CodypendentError::new(
                "analytics.unsupported-grouping",
                "the requested analytics grouping is unsupported",
                false,
            ),
            crate::analytics::AnalyticsError::InvalidCursor => CodypendentError::new(
                "analytics.invalid-cursor",
                "the analytics cursor is invalid",
                false,
            ),
            _ => CodypendentError::new(
                "analytics.export-failed",
                "the analytics export failed",
                true,
            ),
        })?;

        let result_json = serde_json::to_string(&result).map_err(|e| {
            CodypendentError::new("internal.command-apply-failed", e.to_string(), true)
        })?;
        let now_str = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE commands SET status = 'applied', result_json = ?, applied_at = ? WHERE id = ?",
        )
        .bind(result_json)
        .bind(now_str)
        .bind(command.command_id.to_string())
        .execute(pool)
        .await
        .map_err(|e| CodypendentError::new("internal.command-apply-failed", e.to_string(), true))?;

        Ok(CommandOutcome {
            command_id: command.command_id,
            created_session: None,
            created_run: None,
            last_sequence: None,
            newly_applied: true,
        })
    }

    async fn apply_import_bundle(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        request: &codypendent_protocol::bundle::BundleImportRequest,
    ) -> Result<CommandOutcome, CodypendentError> {
        let _receipt = crate::bundles::import(
            pool,
            &self.artifacts_or_default(),
            ctx.principal.uid(),
            ctx.client_id,
            command.command_id,
            &command.idempotency_key,
            request,
        )
        .await?;
        Ok(CommandOutcome {
            command_id: command.command_id,
            created_session: None,
            created_run: None,
            last_sequence: None,
            newly_applied: true,
        })
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

        let mut tx = begin_session_write(pool, session_id).await?;

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
                // `approvals.resolved_by` and the `Actor::Human` on the appended
                // `ApprovalResolved` both come from here. It used to be the
                // client's own UUID, which made the audit trail a record of what
                // the caller typed; it is now the peer uid the kernel reported.
                ctx.principal.user_id().0,
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

        crate::control_plane_sync::outbox::enqueue_session_snapshot(
            &mut tx,
            &session_id.to_string(),
        )
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

    /// Resolve a parked question (adoption 03). Mirrors `apply_resolve_approval`:
    /// session-scoped, idempotent, revision-guarded. Inside the command's own
    /// transaction we flip the row to answered/rejected and append `QuestionResolved`,
    /// bump the revision, mark `commands` applied, and commit; only after
    /// commit do we publish the event and wake the waiter.
    async fn apply_resolve_question(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        question_id: codypendent_protocol::QuestionId,
        outcome: QuestionOutcome,
    ) -> Result<CommandOutcome, CodypendentError> {
        let session_id = question_session(pool, question_id)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| question_not_found(question_id))?;

        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let body_json = serde_json::to_string(&command.body).map_err(internal_error)?;

        let mut tx = begin_session_write(pool, session_id).await?;

        // 1. Command row (received).
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

        // 2. Optimistic concurrency guard.
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

        // 3. Resolve inside tx.
        let event = match self
            .questions
            .resolve_in_tx(
                &mut tx,
                question_id,
                outcome.clone(),
                ctx.principal.user_id().0,
                now,
            )
            .await
        {
            Ok(event) => {
                // 4. Bump session revision.
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
            Err(QuestionError::AlreadyResolved { .. }) => None,
            Err(err @ QuestionError::NotFound { .. }) => {
                let _ = tx.rollback().await;
                return Err(map_question_error(err));
            }
            Err(err) => {
                let _ = tx.rollback().await;
                return Err(map_question_error(err));
            }
        };

        // 5. Outcome and applied update.
        let last_sequence = match &event {
            Some(event) => Some(event.sequence),
            None => tx_max_sequence(&mut *tx, session_id)
                .await
                .map_err(internal_error)?,
        };
        let outcome_dto = CommandOutcome {
            command_id: command.command_id,
            created_session: None,
            created_run: None,
            last_sequence,
            newly_applied: false,
        };
        sqlx::query(
            "UPDATE commands SET status = 'applied', result_json = ?, applied_at = ? WHERE id = ?",
        )
        .bind(serde_json::to_string(&outcome_dto).map_err(internal_error)?)
        .bind(&now_str)
        .bind(command.command_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;

        crate::control_plane_sync::outbox::enqueue_session_snapshot(
            &mut tx,
            &session_id.to_string(),
        )
        .await
        .map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;

        // 6. Post-commit wake + publish.
        if let Some(event) = event {
            let reply = match outcome {
                QuestionOutcome::Answered { answers } => QuestionReply::Answered(answers),
                QuestionOutcome::Rejected { feedback } => QuestionReply::Rejected { feedback },
                _ => QuestionReply::Rejected { feedback: None },
            };
            self.questions.wake(question_id, reply).await;
            self.subscriptions.publish(session_id, event);
        }

        Ok(outcome_dto)
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
                if err.downcast_ref::<SessionClosed>().is_some() {
                    return Err(session_closed());
                }
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

        if matches!(revision, RevisionOp::Bump { .. }) {
            ensure_session_open(&mut tx, event_session).await?;
        }

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
        if let PreInsert::Session {
            session_id,
            title,
            owner_uid,
            workspace_id,
            repository_id,
            repository,
            internal,
            parent_session_id,
            parent_run_id,
        } = pre
        {
            sqlx::query(
                "INSERT INTO sessions \
                 (id, workspace_id, title, state, created_at, updated_at, revision, owner_uid, \
                  repository_id, repository, last_activity_at, internal, parent_session_id, \
                  parent_run_id) \
                 VALUES (?, ?, ?, 'open', ?, ?, 0, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(session_id.to_string())
            .bind(workspace_id.to_string())
            .bind(title)
            .bind(&now_str)
            .bind(&now_str)
            .bind(i64::from(owner_uid))
            .bind(repository_id.map(|id| id.to_string()))
            .bind(repository)
            .bind(&now_str)
            .bind(i64::from(internal))
            .bind(parent_session_id.map(|id| id.to_string()))
            .bind(parent_run_id.map(|id| id.to_string()))
            .execute(&mut *tx)
            .await?;
            crate::session_library::index_title_source(&mut *tx, session_id, title, &now_str)
                .await?;
        }

        // Append events, allocating each sequence inside this tx.
        let mut persisted = Vec::with_capacity(events.len());
        for (actor, body) in events {
            let sequence = next_sequence(&mut *tx, event_session).await?;
            append_event(
                &mut tx,
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
        let mut changed_run = None;
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
                changed_run = Some(run_id);
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
                changed_run = Some(run_id);
            }
        }

        // The local projection and its control-plane delta share this write
        // transaction. A successful command can therefore never leave a
        // publishable session/run mutation stranded outside the durable
        // outbox if the process exits before the next sync cycle.
        crate::control_plane_sync::outbox::enqueue_session_snapshot(
            &mut tx,
            &event_session.to_string(),
        )
        .await?;
        if let Some(run_id) = changed_run {
            crate::control_plane_sync::outbox::enqueue_run_snapshot(&mut tx, &run_id.to_string())
                .await?;
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

        if let Some(session_id) = command_session {
            crate::session_library::index_command_sources(
                &mut tx,
                session_id,
                command.command_id,
                &command.body,
                &now_str,
            )
            .await?;
        }

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
                &mut tx,
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

    /// Apply an idempotent inbox mutation (Acknowledge, Dismiss).
    async fn apply_mutate_inbox(
        &self,
        pool: &SqlitePool,
        ctx: &ApplyContext,
        command: &Command,
        mutation: codypendent_protocol::InboxMutation,
    ) -> Result<CommandOutcome, CodypendentError> {
        if matches!(mutation, codypendent_protocol::InboxMutation::Unknown) {
            return Err(CodypendentError::new(
                "inbox.unsupported-mutation",
                "the requested inbox mutation is unsupported",
                false,
            ));
        }

        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let body_json = serde_json::to_string(&command.body).map_err(internal_error)?;

        let mut tx = pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(internal_error)?;

        // 1. Command row (received) for idempotency.
        if let Err(err) = sqlx::query(
            "INSERT INTO commands \
             (id, idempotency_key, session_id, client_id, body, status, received_at) \
             VALUES (?, ?, NULL, ?, ?, 'received', ?)",
        )
        .bind(command.command_id.to_string())
        .bind(&command.idempotency_key)
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

        // 2. Apply mutation to inbox entry.
        let entry = crate::inbox::apply_mutation(&mut tx, ctx.principal, &mutation, now)
            .await
            .map_err(crate::inbox::into_codypendent_error)?;

        let outcome = CommandOutcome {
            command_id: command.command_id,
            created_session: None,
            created_run: None,
            last_sequence: None,
            newly_applied: false,
        };

        let persisted = crate::inbox::PersistedInboxOutcome {
            outcome: outcome.clone(),
            entry,
        };
        let result_json = serde_json::to_string(&persisted).map_err(internal_error)?;

        sqlx::query(
            "UPDATE commands SET status = 'applied', result_json = ?, applied_at = ? WHERE id = ?",
        )
        .bind(&result_json)
        .bind(&now_str)
        .bind(command.command_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;

        tx.commit().await.map_err(internal_error)?;

        Ok(outcome)
    }
}

// --- free helpers ------------------------------------------------------------

/// Task 1.3 reserves these additive wire contracts before their durable
/// implementations land. Recognized commands fail explicitly instead of
/// looking unknown, role-denied, or successfully accepted as no-ops.
pub(crate) fn is_reserved_unsupported_command(body: &CommandBody) -> bool {
    matches!(
        body,
        CommandBody::MutateSessionLifecycle {
            action: SessionLifecycleAction::Unknown,
            ..
        }
    )
}

pub(crate) fn reserved_unsupported_error() -> CodypendentError {
    CodypendentError::new(
        "protocol.unsupported-payload",
        "command is reserved but not implemented by this daemon",
        false,
    )
}

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
        | CommandBody::QueueSteering { .. }
        | CommandBody::QueuePrompt { .. }
        | CommandBody::UpdateQueuedPrompt { .. }
        | CommandBody::PromoteQueuedPrompt { .. }
        | CommandBody::DeleteQueuedPrompt { .. }
        // Operator-initiated composer actions (adoption 20): a `!` shell escape
        // and a `#` memory quick-add are session mutations the operator drives
        // from the composer, so they carry the same role floor as submitting
        // input or queueing a prompt. The `!` escape is separately confined by
        // the sandbox and its own worktree scope in `apply_run_user_shell`.
        | CommandBody::RunUserShell { .. }
        | CommandBody::RememberMemory { .. }
        | CommandBody::RunEditorAction { .. }
        | CommandBody::QueryAnalytics { .. }
        | CommandBody::MutateInbox { .. } => {
            matches!(role, Contributor | Controller | Approver)
        }
        CommandBody::CancelRun { .. }
        | CommandBody::PauseRun { .. }
        | CommandBody::ResumeRun { .. }
        | CommandBody::CloseSession { .. }
        | CommandBody::ExportBundle { .. }
        | CommandBody::ExportAnalytics { .. }
        | CommandBody::ImportBundle { .. }
        | CommandBody::MutateSessionLifecycle { .. } => matches!(role, Controller),
        CommandBody::ResolveApproval { .. } => matches!(role, Approver | Controller),
        CommandBody::ResolveQuestion { .. } => {
            matches!(role, Contributor | Controller | Approver)
        }
        CommandBody::ManageAutomationBinding { request } => match request {
            AutomationBindingRequest::Get { .. } | AutomationBindingRequest::List { .. } => true,
            AutomationBindingRequest::Create { .. }
            | AutomationBindingRequest::Update { .. }
            | AutomationBindingRequest::Delete { .. } => matches!(role, Controller),
            _ => false,
        },
        // Analytics budgets, decided exactly like automation bindings above:
        // reading one's OWN budgets is a read every attached role may make (the
        // handler scopes every statement to `conn.principal`), while creating,
        // changing or deleting a spend threshold is a control-plane mutation
        // that keeps the `Controller` floor. The inner `_ => false` is the
        // fail-closed arm for a request variant added after this build.
        CommandBody::ManageAnalyticsBudget { request } => match request {
            AnalyticsBudgetRequest::Get { .. } | AnalyticsBudgetRequest::List { .. } => true,
            AnalyticsBudgetRequest::Create { .. }
            | AnalyticsBudgetRequest::Update { .. }
            | AnalyticsBudgetRequest::Delete { .. } => matches!(role, Controller),
            _ => false,
        },
        CommandBody::MarketplaceSearch { .. } | CommandBody::SecretList { .. } => true,
        // A read for every role. The credential spend a networked probe costs
        // is real, but it is the operator's own daemon answering about the
        // operator's own configuration: the ownership gate has already refused
        // a foreign principal, and an Observer that cannot see why a run will
        // not start is the readiness gap this command exists to close.
        CommandBody::ProbeModel { .. } | CommandBody::ReadKnowledge { .. } => true,
        CommandBody::MarketplaceInstall { .. }
        | CommandBody::MarketplaceUpdate { .. }
        | CommandBody::MarketplaceEnable { .. }
        | CommandBody::MarketplaceDisable { .. }
        | CommandBody::MarketplaceRevoke { .. }
        | CommandBody::SecretDeclare { .. }
        | CommandBody::SecretBind { .. }
        | CommandBody::SecretRevoke { .. } => matches!(role, Controller),
        // Reading an artifact is a read, so every attached role may ask —
        // including `Observer`, which exists to watch a session (the VS Code
        // patch-review surface attaches as one). It is NOT unguarded: the
        // ownership gate resolves `NamedResource::Artifact` against
        // `owner_uid` (migration 0039) and answers a generic not-found for
        // anything the principal does not own, and the handler clamps the
        // requested span to `MAX_READ_ARTIFACT_BYTES`.
        CommandBody::ReadArtifact { .. } => true,
        // Listing the inbox is a read of the caller's OWN entries — the handler
        // (`inbox::list_entries`) takes the connection principal and scopes the
        // query to it, so there is nothing here for a lower role to see that a
        // higher one would not. Same floor as `ReadArtifact`: every attached
        // role, including `Observer`, whose whole job is to watch. Decided
        // deliberately and not folded into the `MutateInbox` arm above:
        // acknowledging or dismissing an entry MUTATES it and keeps the
        // contributor floor.
        CommandBody::ListInbox { .. } => true,
        // --- Milestone 6: federation & campaigns ---
        //
        // These bodies are intercepted at the connection level (like the
        // marketplace and secret families above), so this function is not the
        // only thing standing between a client and the handler —
        // `federation::handle` re-applies the identical floor. They are listed
        // here anyway, and in `every_client_issued_command_has_a_decided_role_floor`,
        // because a body that falls through `_ => false` is role-denied for
        // EVERY client and the failure is invisible: the type exists, the
        // handler exists, its unit tests pass, and the feature is simply
        // unreachable on the wire. `RunUserShell`, `RememberMemory`,
        // `ReadArtifact` and `ListInbox` all shipped that way.
        //
        // Reads: every attached role, including `Observer`, whose whole job is
        // to watch. None of these is unguarded — `authorize_command` has
        // already resolved the repository or campaign against this principal,
        // and a graph query returns only rows inside its `AuthorizedGrants`.
        CommandBody::GetPublicationPolicy { .. }
        | CommandBody::QueryFederatedGraph { .. }
        | CommandBody::QueryBlastRadius { .. }
        | CommandBody::PlanMigration { .. }
        | CommandBody::SuggestReviewers { .. }
        | CommandBody::GetCampaign { .. }
        | CommandBody::ListCampaigns { .. } => true,
        // Mutations: `Controller` only.
        //
        // `EstablishFederatedIdentity` mints the durable cross-machine identity
        // every published fact is keyed to. `SetPublicationPolicy` decides what
        // may leave the machine at all. `PublishGraphFacts` is the act of
        // sending it, and `TombstoneGraphFacts` the act of retracting it —
        // neither is a read by any reading, so neither takes the contributor
        // floor session mutations do. `CreateCampaign`/`ExecuteCampaign` fan an
        // ordinary workflow run — arbitrary code — across N repositories, and
        // `CancelCampaign` is a lifecycle mutation, which is the same floor
        // `CancelRun` and `CloseSession` carry.
        CommandBody::EstablishFederatedIdentity { .. }
        | CommandBody::SetPublicationPolicy { .. }
        | CommandBody::PublishGraphFacts { .. }
        | CommandBody::TombstoneGraphFacts { .. }
        | CommandBody::CreateCampaign { .. }
        | CommandBody::ExecuteCampaign { .. }
        | CommandBody::CancelCampaign { .. } => matches!(role, Controller),
        _ => false,
    }
}

/// Run one composer quick-add through the governed memory ledger, so the
/// curator's dedup, contradiction, provenance and retention gates actually
/// decide whether it becomes a durable memory.
///
/// Best-effort by design and by precedent (the executor's post-run harvest does
/// the same): the note this cites is already committed, so a curation failure
/// is logged, never turned into a command failure the operator has to retry.
/// The candidate cites that note event, which is what satisfies the provenance
/// gate — an evidence-free candidate would simply be rejected.
///
/// Called only AFTER the command transaction commits: `curate` opens its own
/// transaction, and nesting one inside a `BEGIN IMMEDIATE` write would deadlock.
async fn curate_quick_add_memory(
    pool: &SqlitePool,
    session_id: SessionId,
    sequence: i64,
    statement: &str,
    observed_at: chrono::DateTime<Utc>,
) {
    use codypendent_knowledge::{
        local_user_scope, CandidateMemory, Curation, EvidenceRef, MemoryClass, MemoryStore,
        Revision,
    };

    let Ok(sequence) = u64::try_from(sequence) else {
        tracing::warn!(%session_id, "quick-add memory cites a negative ledger sequence; not curated");
        return;
    };
    let candidate = CandidateMemory {
        // The cross-repository user scope is reserved for preference-class
        // facts, and an operator typing "remember this" is asserting exactly
        // one. Repository scope is not available here (a session carries no
        // repository until a run takes a workspace lease), and session scope is
        // never queried by `emit_context` — it would store a memory nothing can
        // ever read.
        class: MemoryClass::Preference,
        scope: Some(local_user_scope()),
        statement: statement.trim().to_string(),
        structured_value: None,
        provenance: vec![EvidenceRef::EventRange {
            session_id,
            from_sequence: sequence,
            to_sequence: sequence,
        }],
        // Operator-asserted: above the observer's 0.6 for an inferred fact,
        // below 1.0 — an operator can still be wrong.
        confidence: 0.9,
        observed_at,
        valid_from: Revision::sequence(sequence),
        sensitivity: DataClassification::Internal,
        retention: None,
    };
    match MemoryStore::new().curate(pool, candidate).await {
        // Only the verdict is logged, never the statement.
        Ok(curation) => {
            let verdict = match curation {
                Curation::Accepted(_) => "accepted",
                Curation::Redacted { .. } => "redacted",
                Curation::Duplicate { .. } => "duplicate",
                Curation::Superseded { .. } => "superseded",
                Curation::Rejected { .. } => "rejected",
            };
            tracing::debug!(%session_id, verdict, "curated a composer quick-add memory");
        }
        Err(error) => {
            tracing::warn!(%session_id, %error, "could not curate a composer quick-add memory");
        }
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

/// Run a `!` operator shell escape CONFINED under the session's sandbox profile,
/// returning the model-/operator-facing note text.
///
/// `!` is an operator-initiated shell escape, but codypendent runs it CONFINED:
/// `/bin/sh -c <cmd>` still supports pipes/redirects, yet it executes inside the
/// platform sandbox ([`enforcing_executor`]) scoped to the session worktree
/// (read+write there, no network), bounded (memory/CPU/output) and time-limited
/// (wall-clock kill). It FAILS CLOSED: when the sandbox cannot be enforced
/// ([`SandboxError::UnsupportedPlatform`] / [`SandboxError::ToolUnavailable`],
/// e.g. Linux without bwrap or Windows) the command is REFUSED with a legible
/// note — never run unconfined.
async fn run_user_shell_command(shell_cmd: &str, cwd: &Path, origin: &str) -> String {
    let executor = match enforcing_executor() {
        Ok(executor) => executor,
        // Fail-closed: no OS backend / missing tool ⇒ refuse, never run bare.
        Err(err) => return shell_escape_refusal(&err),
    };
    let shell_cmd = shell_cmd.trim().to_string();
    let cwd = cwd.to_path_buf();
    let origin = origin.to_string();
    // The sandbox executor is blocking (spawns + reaps a child), so offload it
    // off the async worker rather than stalling the runtime.
    match tokio::task::spawn_blocking(move || {
        run_user_shell_confined(executor.as_ref(), &shell_cmd, &cwd, &origin)
    })
    .await
    {
        Ok(note) => note,
        Err(join_err) => format!("shell escape failed: {join_err}"),
    }
}

/// Build the confined command + profile and run it through `executor`, mapping
/// the outcome to note text. Split out (and executor-injected) so the mapping is
/// unit-testable against a mock sandbox.
fn run_user_shell_confined(
    executor: &dyn SandboxExecutor,
    shell_cmd: &str,
    cwd: &Path,
    origin: &str,
) -> String {
    let profile = user_shell_profile(origin, cwd);
    let command = SandboxCommand::new(
        std::path::PathBuf::from("/bin/sh"),
        vec!["-c".to_string(), shell_cmd.to_string()],
        cwd.to_path_buf(),
        origin.to_string(),
    );
    user_shell_note(executor.run(&profile, &command))
}

/// The minimal sandbox profile for an interactive operator shell in the session
/// worktree: read+write the worktree, NO network (unless a session grants one
/// elsewhere — this handler does not), subprocess allowed (a shell pipeline
/// spawns children), a clean env allowlist, and bounded resources. Deliberately
/// no broader grant than an operator editing files in their own worktree needs.
fn user_shell_profile(origin: &str, worktree: &Path) -> SandboxProfile {
    let worktree = worktree.to_string_lossy().to_string();
    SandboxProfile {
        plugin: origin.to_string(),
        env_allowlist: ENV_ALLOWLIST.iter().map(|s| (*s).to_string()).collect(),
        read_paths: vec![worktree.clone()],
        write_paths: vec![worktree],
        network_allowlist: Vec::new(),
        brokered_secrets: Vec::new(),
        allow_subprocess: true,
        memory_mb: USER_SHELL_MEMORY_MB,
        cpu_seconds: USER_SHELL_CPU_SECONDS,
        wall_seconds: USER_SHELL_WALL_SECONDS,
        maximum_output_mb: USER_SHELL_OUTPUT_MB,
    }
}

/// Map a confined-run result to the note recorded on the ledger. The executor
/// already sanitizes and caps output (`maximum_output_mb`), so this just folds
/// stdout+stderr and annotates a timeout or truncation.
fn user_shell_note(result: Result<SandboxOutcome, SandboxError>) -> String {
    match result {
        Ok(outcome) => {
            if outcome.timed_out {
                return format!("(timed out after {USER_SHELL_WALL_SECONDS}s)");
            }
            let mut combined = outcome.stdout.text.clone();
            if !outcome.stderr.text.is_empty() {
                if !combined.is_empty() && !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(&outcome.stderr.text);
            }
            let mut text = combined.trim().to_string();
            if outcome.output_truncated {
                text.push_str("\n... output truncated (exceeded sandbox cap) ...");
            }
            if text.is_empty() {
                format!(
                    "(process exited with code {})",
                    outcome.exit_code.unwrap_or(0)
                )
            } else {
                text
            }
        }
        // Fail-closed: the sandbox refused the run — report it, never re-run bare.
        Err(err) => shell_escape_refusal(&err),
    }
}

/// The fail-closed refusal note: the sandbox could not be enforced (or refused
/// the run), so the `!` command was NOT executed. The [`SandboxError`] display
/// carries the operator-legible diagnostic.
fn shell_escape_refusal(err: &SandboxError) -> String {
    format!("shell escape refused: sandbox enforcement unavailable — {err}")
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

/// Atomic lifecycle guard failure for writes aimed at a terminal session.
#[derive(Debug)]
struct SessionClosed;

impl std::fmt::Display for SessionClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("session is closed")
    }
}

impl std::error::Error for SessionClosed {}

fn session_closed() -> CodypendentError {
    CodypendentError::new("session.closed", "session is closed", false)
}

fn session_has_active_run() -> CodypendentError {
    CodypendentError::new(
        "session.active-run",
        "session has a nonterminal active run",
        true,
    )
}

fn session_run_evidence_pending() -> CodypendentError {
    CodypendentError::new(
        "session.run-evidence-pending",
        "session has a terminal run awaiting completion evidence",
        true,
    )
}

fn session_has_pending_human_work() -> CodypendentError {
    CodypendentError::new(
        "session.pending-human-work",
        "session has unresolved approvals or questions",
        true,
    )
}

fn session_has_effect_in_flight() -> CodypendentError {
    CodypendentError::new(
        "session.effect-in-flight",
        "session has an external effect awaiting durable completion",
        true,
    )
}

fn session_has_received_command() -> CodypendentError {
    CodypendentError::new(
        "session.command-in-flight",
        "session has a command awaiting application",
        true,
    )
}

/// A CloseSession key may replay only that exact session target. This keeps the
/// command principal-owned even when a caller guesses a key used by another
/// command: the server's outer ownership gate authorizes the requested body,
/// and this check prevents the idempotency lookup from substituting a different
/// recorded body after that gate.
fn reject_replay_mismatch(
    requested: &CommandBody,
    recorded_json: &str,
) -> Result<(), CodypendentError> {
    if !matches!(
        requested,
        CommandBody::CloseSession { .. } | CommandBody::MutateSessionLifecycle { .. }
    ) {
        return Ok(());
    }
    let recorded: CommandBody = serde_json::from_str(recorded_json).map_err(internal_error)?;
    if &recorded == requested {
        Ok(())
    } else {
        Err(CodypendentError::new(
            "command.idempotency-conflict",
            "idempotency key was already used for a different command body",
            false,
        ))
    }
}

async fn load_lifecycle_summary_in(
    exec: impl sqlx::SqliteExecutor<'_>,
    session_id: SessionId,
) -> Result<SessionSummary, CodypendentError> {
    type Row = (
        String,
        Option<String>,
        String,
        String,
        String,
        String,
        i64,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let row: Row = sqlx::query_as(
        "SELECT id, workspace_id, title, state, updated_at, created_at, internal, pinned, \
         archived_at, repository_id, repository, last_activity_at FROM sessions \
         WHERE id = ? AND tombstoned_at IS NULL",
    )
    .bind(session_id.to_string())
    .fetch_optional(exec)
    .await
    .map_err(internal_error)?
    .ok_or_else(|| {
        CodypendentError::new(
            "protocol.session-not-found",
            format!("no session {session_id}"),
            false,
        )
    })?;
    let parse_time = |value: &str| {
        chrono::DateTime::parse_from_rfc3339(value)
            .map(|value| value.with_timezone(&Utc))
            .map_err(internal_error)
    };
    Ok(SessionSummary {
        session_id: row.0.parse().map_err(internal_error)?,
        workspace_id: row.1.and_then(|value| value.parse().ok()),
        title: row.2,
        state: row.3,
        updated_at: parse_time(&row.4)?,
        created_at: parse_time(&row.5)?,
        internal: row.6 != 0,
        pinned: row.7 != 0,
        archived_at: row.8.as_deref().map(parse_time).transpose()?,
        repository_id: row.9.and_then(|value| value.parse().ok()),
        repository: row.10,
        last_activity_at: row.11.as_deref().map(parse_time).transpose()?,
        parent_session_id: None,
        parent_run_id: None,
        workspace: None,
        last_run_id: None,
        run_state: None,
    })
}

/// Begin the established serialized write transaction and reject a terminal
/// session while holding that same lock. This makes close-vs-write ordering
/// atomic: whichever transaction commits first determines the outcome.
async fn begin_session_write(
    pool: &SqlitePool,
    session_id: SessionId,
) -> Result<Transaction<'_, Sqlite>, CodypendentError> {
    let mut tx = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(internal_error)?;
    ensure_session_open(&mut tx, session_id)
        .await
        .map_err(|error| {
            if error.downcast_ref::<SessionClosed>().is_some() {
                session_closed()
            } else {
                internal_error(error)
            }
        })?;
    Ok(tx)
}

async fn ensure_session_open(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: SessionId,
) -> anyhow::Result<()> {
    let (state, tombstoned_at): (String, Option<String>) =
        sqlx::query_as("SELECT state, tombstoned_at FROM sessions WHERE id = ?")
            .bind(session_id.to_string())
            .fetch_one(&mut **tx)
            .await?;
    if state == "closed" || tombstoned_at.is_some() {
        return Err(SessionClosed.into());
    }
    Ok(())
}

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
        ApprovalError::PatternUnavailable => {
            CodypendentError::new("approval.pattern-unavailable", err.to_string(), false)
        }
        other => internal_error(other),
    }
}

fn map_question_error(err: QuestionError) -> CodypendentError {
    match err {
        QuestionError::NotFound { .. } => {
            CodypendentError::new("question.not-found", err.to_string(), false)
        }
        QuestionError::AlreadyResolved { .. } => {
            CodypendentError::new("question.already-resolved", err.to_string(), false)
        }
        QuestionError::UnsupportedOutcome => {
            CodypendentError::new("protocol.unsupported-payload", err.to_string(), false)
        }
        other => internal_error(other),
    }
}

async fn question_session(
    pool: &SqlitePool,
    question_id: QuestionId,
) -> anyhow::Result<Option<SessionId>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT r.session_id FROM questions q JOIN runs r ON q.run_id = r.id WHERE q.id = ?",
    )
    .bind(question_id.to_string())
    .fetch_optional(pool)
    .await?;
    match row {
        Some((id_str,)) => Ok(Some(SessionId::from_str(&id_str)?)),
        None => Ok(None),
    }
}

fn question_not_found(question_id: QuestionId) -> CodypendentError {
    CodypendentError::new(
        "question.not-found",
        format!("no question with id {question_id}"),
        false,
    )
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

async fn active_session_exists(pool: &SqlitePool, session_id: SessionId) -> anyhow::Result<bool> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM sessions WHERE id = ? AND tombstoned_at IS NULL")
            .bind(session_id.to_string())
            .fetch_optional(pool)
            .await?;
    Ok(row.is_some())
}

/// The `owner_uid` of a session (migration 0031), for a fork re-driven on
/// recovery to inherit its source's owner. `None` when the session is missing or
/// its row predates the column.
async fn session_owner_uid(
    pool: &SqlitePool,
    session_id: SessionId,
) -> anyhow::Result<Option<u32>> {
    let row: Option<(Option<i64>,)> = sqlx::query_as("SELECT owner_uid FROM sessions WHERE id = ?")
        .bind(session_id.to_string())
        .fetch_optional(pool)
        .await?;
    Ok(row
        .and_then(|(uid,)| uid)
        .and_then(|uid| u32::try_from(uid).ok()))
}

/// The repository and pinned model a session's runs inherit, recovered from the
/// session's originating [`StartRun`](CommandBody::StartRun) command
/// (continuous-session I-1/I-2).
///
/// Neither value is projected onto the `runs` row: the row carries no repository
/// column at all, and only a *default* `model_policy` ([`DEFAULT_MODEL_POLICY`],
/// supplied by the write path) — never the operator's pin. The authoritative
/// source is the persisted `StartRun` command body itself, which the idempotent
/// write path stores verbatim, so the operator's `repository`/`model` survive on
/// it. A follow-up ([`SubmitUserInput`](CommandBody::SubmitUserInput)) carries
/// neither on the wire, so a continuation reads them from here to run against the
/// SAME checkout and pinned model as the session's first run — instead of the
/// daemon's (possibly startup-frozen) `current_dir()` and an unpinned default.
#[derive(Debug, Default, Clone)]
pub struct SessionRunProvenance {
    /// The repository root the session's originating run was launched against
    /// (`StartRun.repository`); `None` when that run carried none (an older
    /// client) or the session has no applied `StartRun`.
    pub repository: Option<String>,
    /// The model the session's originating run pinned (`StartRun.model`); `None`
    /// when unpinned or the session has no applied `StartRun`.
    pub model: Option<ModelId>,
}

/// Recover a session's [`SessionRunProvenance`] from its applied command ledger.
///
/// **Model** (the pin): the most recent command that carried one — a `StartRun`
/// *or* a mid-conversation `SubmitUserInput` re-pin — wins, scanning newest-first.
/// So a session re-pinned mid-conversation inherits its LATEST pin (the switch
/// sticks for the next follow-up), while a follow-up that carried none never
/// clobbers the session's current model. A `SubmitUserInput` with no pin
/// (`model: None`) is transparent to this scan.
///
/// **Repository** (stable across a session): only a `StartRun` carries one, so
/// this takes the most recent `StartRun`'s `repository` — unchanged from before
/// this function also considered continuations for the model.
///
/// A session with no applied `StartRun`/pin yields the default (both `None`), so
/// the caller falls back exactly as an older client's continuation did.
///
/// The `body LIKE` clause only *bounds* the rows scanned — the command body is
/// compact JSON, internally tagged `"type":"StartRun"` / `"type":"SubmitUserInput"`
/// — while the deserialize-and-match below is the authoritative extractor.
pub async fn session_run_provenance(
    pool: &SqlitePool,
    session_id: SessionId,
) -> anyhow::Result<SessionRunProvenance> {
    let mut connection = pool.acquire().await?;
    let mut provenance = SessionRunProvenance::default();
    let mut model_found = false;
    let mut repository_found = false;
    session_run_provenance_inner(
        &mut connection,
        session_id,
        0,
        i64::MAX,
        &mut provenance,
        &mut model_found,
        &mut repository_found,
    )
    .await?;
    Ok(provenance)
}

async fn session_run_provenance_inner(
    connection: &mut sqlx::SqliteConnection,
    session_id: SessionId,
    depth: usize,
    max_command_rowid: i64,
    provenance: &mut SessionRunProvenance,
    model_found: &mut bool,
    repository_found: &mut bool,
) -> anyhow::Result<()> {
    if depth > 8 {
        return Ok(());
    }
    let bodies: Vec<(String,)> = sqlx::query_as(
        "SELECT body FROM commands \
         WHERE session_id = ? AND status = 'applied' \
           AND rowid <= ? \
           AND (body LIKE '%\"type\":\"StartRun\"%' \
                OR body LIKE '%\"type\":\"SubmitUserInput\"%') \
         ORDER BY rowid DESC",
    )
    .bind(session_id.to_string())
    .bind(max_command_rowid)
    .fetch_all(&mut *connection)
    .await?;
    for (body,) in bodies {
        let command_body = serde_json::from_str::<CommandBody>(&body).map_err(|error| {
            anyhow::anyhow!(
                "applied run-provenance command for session {session_id} is invalid: {error}"
            )
        })?;
        match command_body {
            // A `StartRun` is authoritative for the repository, and (like any
            // pinned command) supplies the model when no newer pin was found.
            CommandBody::StartRun {
                repository, model, ..
            } => {
                if !*model_found {
                    provenance.model = model;
                    *model_found = true;
                }
                if !*repository_found {
                    provenance.repository = repository;
                    *repository_found = true;
                }
            }
            // A mid-conversation re-pin carries no repository. It only overrides
            // the model, and only when it actually pinned one — a `None`
            // follow-up (matched by the catch-all below) is transparent and must
            // NOT clobber the session's model.
            CommandBody::SubmitUserInput {
                model: Some(model), ..
            } if !*model_found => {
                provenance.model = Some(model);
                *model_found = true;
            }
            _ => {}
        }
        if *model_found && *repository_found {
            return Ok(());
        }
    }
    if !*model_found || !*repository_found {
        let parent_row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT forked_from_session_id FROM sessions WHERE id = ?")
                .bind(session_id.to_string())
                .fetch_optional(&mut *connection)
                .await?;

        if let Some((Some(parent_id_str),)) = parent_row {
            if let Ok(parent_id) = SessionId::from_str(&parent_id_str) {
                Box::pin(session_run_provenance_inner(
                    connection,
                    parent_id,
                    depth + 1,
                    max_command_rowid,
                    provenance,
                    model_found,
                    repository_found,
                ))
                .await?;
            }
        }
    }
    Ok(())
}

/// Recover the repository and model that an already-created run must use when
/// it is dispatched or re-driven after a crash.
///
/// The originating command is authoritative for a run-specific model override.
/// Continuations and editor actions do not carry a repository, so they inherit
/// the session's `StartRun.repository` as it existed when that originating
/// command was durably inserted. Commands with a newer SQLite rowid are ignored:
/// a later repository/model re-pin must not rewrite the identity of an already
/// queued run. Database and deserialization failures are returned to the caller:
/// callers must fail the run rather than silently treating an unavailable
/// provenance ledger as an old unbound session and executing in daemon cwd.
pub async fn run_launch_provenance(
    pool: &SqlitePool,
    run_id: RunId,
    session_id: SessionId,
) -> anyhow::Result<SessionRunProvenance> {
    let mut connection = pool.acquire().await?;
    run_launch_provenance_in(&mut connection, run_id, session_id).await
}

pub(crate) async fn run_launch_provenance_in(
    connection: &mut sqlx::SqliteConnection,
    run_id: RunId,
    session_id: SessionId,
) -> anyhow::Result<SessionRunProvenance> {
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT rowid, body FROM commands \
         WHERE status = 'applied' AND json_extract(result_json, '$.created_run') = ?",
    )
    .bind(run_id.to_string())
    .fetch_optional(&mut *connection)
    .await?;

    let Some((origin_rowid, body_json)) = row else {
        // Every run accepted by the current command path commits its applied
        // command and `created_run` outcome in the same transaction as the run
        // row. Missing provenance is therefore an integrity failure, not proof
        // that daemon cwd is the intended checkout. Older/imported rows without
        // that evidence cannot be executed safely either: fail closed and leave
        // the caller to record a legible terminal outcome.
        anyhow::bail!("no originating command provenance for run {run_id}");
    };
    let body = serde_json::from_str::<CommandBody>(&body_json).map_err(|error| {
        anyhow::anyhow!("originating command for run {run_id} is invalid: {error}")
    })?;

    match body {
        CommandBody::StartRun {
            session_id: command_session,
            repository,
            model,
            ..
        } if command_session == session_id => Ok(SessionRunProvenance { repository, model }),
        CommandBody::SubmitUserInput {
            session_id: command_session,
            model,
            ..
        }
        | CommandBody::RunEditorAction {
            session_id: command_session,
            model,
            ..
        } if command_session == session_id => {
            let mut inherited = SessionRunProvenance::default();
            let mut model_found = false;
            let mut repository_found = false;
            session_run_provenance_inner(
                connection,
                session_id,
                0,
                origin_rowid,
                &mut inherited,
                &mut model_found,
                &mut repository_found,
            )
            .await?;
            Ok(SessionRunProvenance {
                repository: inherited.repository,
                // The run's own override wins. `session_run_provenance` also
                // sees the just-applied command, but keeping this explicit
                // prevents future scan changes from dropping the override.
                model: model.or(inherited.model),
            })
        }
        CommandBody::StartRun { .. }
        | CommandBody::SubmitUserInput { .. }
        | CommandBody::RunEditorAction { .. } => {
            anyhow::bail!("originating command for run {run_id} belongs to a different session")
        }
        _ => anyhow::bail!("originating command for run {run_id} is not a run command"),
    }
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
/// Atomically allocate the next event sequence for `session_id`. `BEGIN
/// IMMEDIATE` is required on the enclosing transaction so no other writer
/// interleaves between this SELECT and the append that claims it.
pub(crate) async fn next_sequence(
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
pub(crate) async fn append_event(
    exec: &mut SqliteConnection,
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
    .execute(&mut *exec)
    .await?;
    crate::session_library::index_event_sources(exec, session_id, sequence, body, occurred_at)
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
        /// The creating principal's uid, recorded so every later by-id read and
        /// every subscription can re-derive permission from what the *server*
        /// stored rather than from what the request claims (outcome 19).
        owner_uid: u32,
        workspace_id: WorkspaceId,
        repository_id: Option<RepositoryId>,
        repository: Option<&'a str>,
        /// Council/fork bookkeeping sessions are hidden from the default
        /// session-library listing (migration 0040 `sessions.internal`).
        internal: bool,
        /// The session this one was spawned from, when any (0040).
        parent_session_id: Option<SessionId>,
        /// The run that spawned this session, when any (0040).
        parent_run_id: Option<RunId>,
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
    use codypendent_protocol::{
        AgentMode, ApprovalDecision, ApprovalId, ApprovalScope, ArtifactId, ArtifactRef,
        DataClassification, RunDisposition,
    };
    use std::path::Path;
    use tempfile::tempdir;

    async fn test_pool(dir: &Path) -> SqlitePool {
        crate::db::open_database(&dir.join("test.db"))
            .await
            .expect("open database")
    }

    /// The unit tests here drive the write path directly, without a socket, so
    /// they name a principal explicitly. Ownership *enforcement* lives at the
    /// wire boundary (`server::authorize_command`) and is covered by
    /// `tests/multi_user_it.rs` against a real daemon.
    const TEST_UID: u32 = 4242;

    fn ctx(role: ClientRole) -> ApplyContext {
        ApplyContext {
            client_id: ClientId::new(),
            role,
            principal: PeerPrincipal::from_uid(TEST_UID),
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
                        repository: None,
                        internal: false,
                        parent_session_id: None,
                        parent_run_id: None,
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

    async fn append_run_completed(
        pool: &SqlitePool,
        session_id: SessionId,
        run_id: RunId,
        disposition: RunDisposition,
    ) -> SessionEvent {
        crate::ledger::append_next_event(
            pool,
            session_id,
            &Actor::System,
            &EventBody::RunCompleted {
                run_id,
                disposition,
                chronicle: ArtifactRef {
                    id: ArtifactId::new(),
                    media_type: "application/json".into(),
                    byte_length: 2,
                    sha256: "0".repeat(64),
                    sensitivity: DataClassification::Internal,
                },
            },
            Utc::now(),
        )
        .await
        .expect("append authoritative RunCompleted evidence")
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
                model: None,
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
    async fn lifecycle_retry_replays_snapshot_after_later_mutation() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "lifecycle-replay-create").await;

        let rename = command(
            CommandBody::MutateSessionLifecycle {
                session_id: session,
                action: SessionLifecycleAction::Rename {
                    title: "snapshot title".into(),
                },
            },
            "lifecycle-replay-key",
        );
        let first = processor
            .apply(&pool, ctx(ClientRole::Controller), rename.clone())
            .await
            .expect("rename session");
        assert!(first.newly_applied);
        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::MutateSessionLifecycle {
                        session_id: session,
                        action: SessionLifecycleAction::Rename {
                            title: "later title".into(),
                        },
                    },
                    "lifecycle-later-key",
                ),
            )
            .await
            .expect("later rename");

        let replay = processor
            .apply(&pool, ctx(ClientRole::Controller), rename)
            .await
            .expect("replay rename");
        let first_response = lifecycle_response(&pool, "lifecycle-replay-key")
            .await
            .expect("persisted first response");
        let replay_response = lifecycle_response(&pool, "lifecycle-replay-key")
            .await
            .expect("persisted replay response");
        assert_eq!(replay_response, first_response);
        let LifecycleResponse::Summary(summary) = replay_response else {
            panic!("rename persisted a summary snapshot");
        };
        assert_eq!(summary.title, "snapshot title");
        assert!(!replay.newly_applied);

        let deleted_session = create_session(&processor, &pool, "delete-replay-create").await;
        let delete = command(
            CommandBody::MutateSessionLifecycle {
                session_id: deleted_session,
                action: SessionLifecycleAction::Delete {
                    mode: SessionDeletionMode::TombstoneOnly,
                },
            },
            "delete-replay-key",
        );
        processor
            .apply(&pool, ctx(ClientRole::Controller), delete.clone())
            .await
            .expect("delete session");
        let receipt = lifecycle_response(&pool, "delete-replay-key")
            .await
            .expect("persisted deletion receipt");
        processor
            .apply(&pool, ctx(ClientRole::Controller), delete)
            .await
            .expect("replay deletion");
        assert_eq!(
            lifecycle_response(&pool, "delete-replay-key")
                .await
                .expect("replayed deletion receipt"),
            receipt
        );
        assert_eq!(
            receipt,
            LifecycleResponse::Deleted {
                session_id: deleted_session,
                tombstoned: true,
            }
        );
    }

    #[tokio::test]
    async fn lifecycle_retry_rejects_key_reused_for_different_body() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "lifecycle-conflict-create").await;
        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::MutateSessionLifecycle {
                        session_id: session,
                        action: SessionLifecycleAction::Pin,
                    },
                    "lifecycle-conflict-key",
                ),
            )
            .await
            .expect("pin session");

        let error = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::MutateSessionLifecycle {
                        session_id: session,
                        action: SessionLifecycleAction::Archive,
                    },
                    "lifecycle-conflict-key",
                ),
            )
            .await
            .expect_err("different lifecycle body must conflict");
        assert_eq!(error.code, "command.idempotency-conflict");
    }

    #[tokio::test]
    async fn close_session_is_atomic_semantically_idempotent_and_preserves_history() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "close-create").await;

        let close_ctx = ctx(ClientRole::Controller);
        let close_client = close_ctx.client_id;
        let close = command(
            CommandBody::CloseSession {
                session_id: session,
            },
            "close-1",
        );
        let first = processor
            .apply(&pool, close_ctx.clone(), close.clone())
            .await
            .expect("first close");
        let replay = processor
            .apply(&pool, close_ctx, close)
            .await
            .expect("same-key replay");
        assert_eq!(first.last_sequence, replay.last_sequence);
        assert!(!replay.newly_applied);

        // `expected_revision` guards only open -> closed. Once closed, even a
        // stale revision under a new key is an accepted no-op and does not bump.
        let mut second_key = command(
            CommandBody::CloseSession {
                session_id: session,
            },
            "close-2",
        );
        second_key.expected_revision = Some(0);
        let noop = processor
            .apply(&pool, ctx(ClientRole::Controller), second_key)
            .await
            .expect("already closed is an unconditional no-op");
        assert_eq!(noop.last_sequence, None);

        let events = crate::ledger::load_events(&pool, session).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.body, EventBody::SessionClosed))
                .count(),
            1
        );
        assert!(
            matches!(
                events.last().unwrap().actor,
                Actor::Client { client_id } if client_id == close_client
            ),
            "closure retains issuing-client attribution"
        );
        let (state, revision): (String, i64) =
            sqlx::query_as("SELECT state, revision FROM sessions WHERE id = ?")
                .bind(session.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, "closed");
        assert_eq!(revision, 1);
        assert!(
            projections::session_projection(&pool, session)
                .await
                .unwrap()
                .closed
        );
        assert_eq!(
            events.len(),
            2,
            "SessionCreated remains readable after close"
        );
    }

    #[tokio::test]
    async fn close_session_rejects_a_key_recorded_for_another_command_body() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let first = create_session(&processor, &pool, "first-close-target").await;
        let second = create_session(&processor, &pool, "second-close-target").await;

        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::CloseSession { session_id: first },
                    "shared-close-key",
                ),
            )
            .await
            .expect("first target closes");
        let error = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::CloseSession { session_id: second },
                    "shared-close-key",
                ),
            )
            .await
            .expect_err("a key cannot substitute another close target");
        assert_eq!(error.code, "command.idempotency-conflict");
        assert!(!error.retryable);
        let (state,): (String,) = sqlx::query_as("SELECT state FROM sessions WHERE id = ?")
            .bind(second.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "open");
    }

    #[tokio::test]
    async fn close_session_rejects_running_and_waiting_runs_until_terminal() {
        for (index, active_state) in [
            RunState::Running,
            RunState::WaitingForApproval,
            RunState::WaitingForUserInput,
        ]
        .into_iter()
        .enumerate()
        {
            let dir = tempdir().unwrap();
            let pool = test_pool(dir.path()).await;
            let processor = CommandProcessor::default();
            let session =
                create_session(&processor, &pool, &format!("active-run-create-{index}")).await;
            let run_id = RunId::new();
            projections::insert_run(
                &pool,
                run_id,
                session,
                "still live",
                AgentMode::Build,
                "default",
                "{}",
            )
            .await
            .unwrap();
            projections::set_run_state(&pool, run_id, active_state)
                .await
                .unwrap();

            let rejected = command(
                CommandBody::CloseSession {
                    session_id: session,
                },
                &format!("active-run-close-{index}"),
            );
            let rejected_id = rejected.command_id;
            let error = processor
                .apply(&pool, ctx(ClientRole::Controller), rejected)
                .await
                .expect_err("a live run prevents closure");
            assert_eq!(error.code, "session.active-run");
            assert!(error.retryable);
            let (state,): (String,) = sqlx::query_as("SELECT state FROM sessions WHERE id = ?")
                .bind(session.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(state, "open");
            let (command_count,): (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM commands WHERE id = ?")
                    .bind(rejected_id.to_string())
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(command_count, 0, "rejected close leaves no command row");
            assert_eq!(
                crate::ledger::load_events(&pool, session)
                    .await
                    .unwrap()
                    .len(),
                1,
                "rejected close appends no event"
            );

            projections::set_run_state(&pool, run_id, RunState::Completed)
                .await
                .unwrap();
            append_run_completed(
                &pool,
                session,
                run_id,
                RunDisposition::Completed { summary: None },
            )
            .await;
            processor
                .apply(
                    &pool,
                    ctx(ClientRole::Controller),
                    command(
                        CommandBody::CloseSession {
                            session_id: session,
                        },
                        &format!("terminal-run-close-{index}"),
                    ),
                )
                .await
                .expect("terminal run permits closure");
        }
    }

    #[tokio::test]
    async fn terminal_run_cannot_close_until_run_completed_is_durable_and_ordered_before_close() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "terminal-evidence-create").await;
        let run_id = RunId::new();
        projections::insert_run(
            &pool,
            run_id,
            session,
            "complete with evidence",
            AgentMode::Build,
            "default",
            "{}",
        )
        .await
        .unwrap();
        crate::ledger::append_run_state_changed(
            &pool,
            session,
            &Actor::System,
            run_id,
            RunState::Completed,
            Utc::now(),
        )
        .await
        .expect("append terminal state transition");

        let error = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::CloseSession {
                        session_id: session,
                    },
                    "close-before-evidence",
                ),
            )
            .await
            .expect_err("terminal projection alone is insufficient");
        assert_eq!(error.code, "session.run-evidence-pending");
        assert!(error.retryable);

        let completed = append_run_completed(
            &pool,
            session,
            run_id,
            RunDisposition::Completed {
                summary: Some("done".into()),
            },
        )
        .await;
        let closed = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::CloseSession {
                        session_id: session,
                    },
                    "close-after-evidence",
                ),
            )
            .await
            .expect("completion evidence permits closure");
        assert!(
            closed.last_sequence.expect("SessionClosed sequence") > completed.sequence,
            "SessionClosed must be strictly after authoritative RunCompleted"
        );
    }

    #[tokio::test]
    async fn cancellation_cannot_close_or_lose_completion_evidence_in_terminal_window() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "cancel-evidence-create").await;
        let run_id = RunId::new();
        projections::insert_run(
            &pool,
            run_id,
            session,
            "cancel safely",
            AgentMode::Build,
            "default",
            "{}",
        )
        .await
        .unwrap();
        crate::ledger::append_run_state_changed(
            &pool,
            session,
            &Actor::System,
            run_id,
            RunState::Cancelled,
            Utc::now(),
        )
        .await
        .expect("append cancellation state");

        let error = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::CloseSession {
                        session_id: session,
                    },
                    "cancel-close-before-evidence",
                ),
            )
            .await
            .expect_err("close cannot interleave before cancellation evidence");
        assert_eq!(error.code, "session.run-evidence-pending");
        assert!(error.retryable);

        let completed = append_run_completed(
            &pool,
            session,
            run_id,
            RunDisposition::Cancelled {
                reason: Some("operator requested".into()),
            },
        )
        .await;
        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::CloseSession {
                        session_id: session,
                    },
                    "cancel-close-after-evidence",
                ),
            )
            .await
            .expect("persisted cancellation evidence permits closure");
        let events = crate::ledger::load_events(&pool, session).await.unwrap();
        assert!(matches!(
            events.iter().find(|event| event.sequence == completed.sequence).map(|event| &event.body),
            Some(EventBody::RunCompleted { run_id: persisted, .. }) if *persisted == run_id
        ));
        assert!(matches!(
            events.last().map(|event| &event.body),
            Some(EventBody::SessionClosed)
        ));
    }

    #[tokio::test]
    async fn close_session_waits_for_pending_approval_and_question_housekeeping() {
        for pending_kind in ["approval", "question"] {
            let dir = tempdir().unwrap();
            let pool = test_pool(dir.path()).await;
            let processor = CommandProcessor::default();
            let session =
                create_session(&processor, &pool, &format!("pending-{pending_kind}-create")).await;
            let run_id = RunId::new();
            projections::insert_run(
                &pool,
                run_id,
                session,
                "settle human work",
                AgentMode::Build,
                "default",
                "{}",
            )
            .await
            .unwrap();
            projections::set_run_state(&pool, run_id, RunState::Completed)
                .await
                .unwrap();
            append_run_completed(
                &pool,
                session,
                run_id,
                RunDisposition::Completed { summary: None },
            )
            .await;

            if pending_kind == "approval" {
                sqlx::query(
                    "INSERT INTO approvals \
                     (id, run_id, action_json, risk_json, capabilities_json, state, scope, requested_at) \
                     VALUES (?, ?, '{}', '{}', '[]', 'pending', 'once', ?)",
                )
                .bind(ApprovalId::new().to_string())
                .bind(run_id.to_string())
                .bind(Utc::now().to_rfc3339())
                .execute(&pool)
                .await
                .unwrap();
            } else {
                sqlx::query(
                    "INSERT INTO questions (id, run_id, questions_json, state, asked_at) \
                     VALUES (?, ?, '[]', 'pending', ?)",
                )
                .bind(QuestionId::new().to_string())
                .bind(run_id.to_string())
                .bind(Utc::now().to_rfc3339())
                .execute(&pool)
                .await
                .unwrap();
            }

            let error = processor
                .apply(
                    &pool,
                    ctx(ClientRole::Controller),
                    command(
                        CommandBody::CloseSession {
                            session_id: session,
                        },
                        &format!("pending-{pending_kind}-close"),
                    ),
                )
                .await
                .expect_err("unsettled human work prevents closure");
            assert_eq!(error.code, "session.pending-human-work");
            assert!(error.retryable);

            let settle = if pending_kind == "approval" {
                "UPDATE approvals SET state = 'expired' WHERE run_id = ?"
            } else {
                "UPDATE questions SET state = 'expired' WHERE run_id = ?"
            };
            sqlx::query(settle)
                .bind(run_id.to_string())
                .execute(&pool)
                .await
                .unwrap();
            processor
                .apply(
                    &pool,
                    ctx(ClientRole::Controller),
                    command(
                        CommandBody::CloseSession {
                            session_id: session,
                        },
                        &format!("settled-{pending_kind}-close"),
                    ),
                )
                .await
                .expect("settled human work permits closure");
        }
    }

    #[tokio::test]
    async fn close_session_rejects_received_command_until_applied() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "received-create").await;
        let in_flight_id = CommandId::new();
        sqlx::query(
            "INSERT INTO commands \
             (id, idempotency_key, session_id, client_id, body, status, received_at) \
             VALUES (?, 'in-flight', ?, 'client', '{}', 'received', ?)",
        )
        .bind(in_flight_id.to_string())
        .bind(session.to_string())
        .bind(Utc::now().to_rfc3339())
        .execute(&pool)
        .await
        .unwrap();

        let rejected = command(
            CommandBody::CloseSession {
                session_id: session,
            },
            "received-close",
        );
        let rejected_id = rejected.command_id;
        let error = processor
            .apply(&pool, ctx(ClientRole::Controller), rejected)
            .await
            .expect_err("received command prevents closure");
        assert_eq!(error.code, "session.command-in-flight");
        assert!(error.retryable);
        let (rejected_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM commands WHERE id = ?")
                .bind(rejected_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rejected_count, 0, "rejected close leaves no command row");
        assert_eq!(
            crate::ledger::load_events(&pool, session)
                .await
                .unwrap()
                .len(),
            1,
            "rejected close appends no event"
        );

        sqlx::query("UPDATE commands SET status = 'applied', applied_at = ? WHERE id = ?")
            .bind(Utc::now().to_rfc3339())
            .bind(in_flight_id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::CloseSession {
                        session_id: session,
                    },
                    "applied-close",
                ),
            )
            .await
            .expect("applied command permits closure");
    }

    #[tokio::test]
    async fn concurrent_different_key_closes_emit_one_event_and_controller_is_required() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "concurrent-close-create").await;

        let denied = processor
            .apply(
                &pool,
                ctx(ClientRole::Contributor),
                command(
                    CommandBody::CloseSession {
                        session_id: session,
                    },
                    "denied-close",
                ),
            )
            .await
            .expect_err("contributors cannot close sessions");
        assert_eq!(denied.code, "protocol.role-denied");

        let one = processor.apply(
            &pool,
            ctx(ClientRole::Controller),
            command(
                CommandBody::CloseSession {
                    session_id: session,
                },
                "race-close-1",
            ),
        );
        let two = processor.apply(
            &pool,
            ctx(ClientRole::Controller),
            command(
                CommandBody::CloseSession {
                    session_id: session,
                },
                "race-close-2",
            ),
        );
        let (one, two) = tokio::join!(one, two);
        one.expect("first racer accepted");
        two.expect("second racer accepted");
        let events = crate::ledger::load_events(&pool, session).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.body, EventBody::SessionClosed))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn closed_session_rejects_representative_writes_without_appending() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "closed-writes-create").await;
        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::CloseSession {
                        session_id: session,
                    },
                    "close",
                ),
            )
            .await
            .unwrap();

        let writes = [
            CommandBody::StartRun {
                session_id: session,
                objective: "late run".into(),
                mode: AgentMode::Build,
                repository: None,
                model: None,
            },
            CommandBody::SubmitUserInput {
                session_id: session,
                text: "late input".into(),
                mode: AgentMode::Build,
                model: None,
                envelope: None,
            },
            CommandBody::QueuePrompt {
                session_id: session,
                text: "late prompt".into(),
                mode: AgentMode::Build,
                delivery: PromptDelivery::Queue,
            },
            CommandBody::RememberMemory {
                session_id: session,
                text: "late memory".into(),
            },
        ];
        for (index, body) in writes.into_iter().enumerate() {
            let error = processor
                .apply(
                    &pool,
                    ctx(ClientRole::Controller),
                    command(body, &format!("closed-write-{index}")),
                )
                .await
                .expect_err("closed session must reject writes");
            assert_eq!(error.code, "session.closed");
            assert_eq!(error.message, "session is closed");
            assert!(!error.retryable);
        }
        let events = crate::ledger::load_events(&pool, session).await.unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events.last().unwrap().body,
            EventBody::SessionClosed
        ));
    }

    #[tokio::test]
    async fn concurrent_close_and_start_never_append_after_session_closed() {
        for iteration in 0..20 {
            let dir = tempdir().unwrap();
            let pool = test_pool(dir.path()).await;
            let processor = CommandProcessor::default();
            let session = create_session(&processor, &pool, "close-start-race").await;
            let close = processor.apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::CloseSession {
                        session_id: session,
                    },
                    &format!("close-{iteration}"),
                ),
            );
            let start = processor.apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::StartRun {
                        session_id: session,
                        objective: "race".into(),
                        mode: AgentMode::Build,
                        repository: None,
                        model: None,
                    },
                    &format!("start-{iteration}"),
                ),
            );
            let (close_result, start_result) = tokio::join!(close, start);
            let events = crate::ledger::load_events(&pool, session).await.unwrap();
            match (close_result, start_result) {
                (Ok(_), Err(error)) => {
                    assert_eq!(error.code, "session.closed");
                    let closed_at = events
                        .iter()
                        .position(|event| matches!(event.body, EventBody::SessionClosed))
                        .expect("closure event");
                    assert_eq!(
                        closed_at,
                        events.len() - 1,
                        "nothing may append after closure"
                    );
                }
                (Err(error), Ok(_)) => {
                    assert_eq!(error.code, "session.active-run");
                    assert!(
                        events
                            .iter()
                            .all(|event| !matches!(event.body, EventBody::SessionClosed)),
                        "a winning run prevents closure"
                    );
                }
                results => panic!("exactly one racing transition must win: {results:?}"),
            }
        }
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
                        model: None,
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
                        model: None,
                        envelope: None,
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

        // Published events arrive in order: the StartRun's RunStarted, then the
        // SubmitUserInput's RunStarted (a follow-up now launches its OWN run —
        // continuous-session plan, Task 3 — rather than recording a bare note).
        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        assert!(matches!(first.body, EventBody::RunStarted { .. }));
        assert!(matches!(second.body, EventBody::RunStarted { .. }));
        assert!(first.sequence < second.sequence);
    }

    #[tokio::test]
    async fn session_run_provenance_recovers_repository_and_pinned_model() {
        // I-1/I-2: a session's repository and pinned model live only on its
        // originating `StartRun` command body (the `runs` row carries neither),
        // and must be recoverable so a later continuation inherits them.
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "prov-create").await;

        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::StartRun {
                        session_id: session,
                        objective: "diagnose".to_string(),
                        mode: AgentMode::Build,
                        repository: Some("/work/some-checkout".to_string()),
                        model: Some(ModelId("pinned-model-x".to_string())),
                    },
                    "prov-start",
                ),
            )
            .await
            .expect("start run");

        // A follow-up carries neither repository nor model, and must NOT clobber
        // the session's recoverable provenance.
        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::SubmitUserInput {
                        session_id: session,
                        text: "keep going".to_string(),
                        mode: AgentMode::Build,
                        model: None,
                        envelope: None,
                    },
                    "prov-input",
                ),
            )
            .await
            .expect("submit input");

        let provenance = session_run_provenance(&pool, session)
            .await
            .expect("recover provenance");
        assert_eq!(
            provenance.repository.as_deref(),
            Some("/work/some-checkout"),
            "the continuation must inherit the session's StartRun repository"
        );
        assert_eq!(
            provenance.model,
            Some(ModelId("pinned-model-x".to_string())),
            "the continuation must inherit the session's pinned model"
        );
    }

    #[tokio::test]
    async fn session_run_provenance_follows_a_mid_conversation_repin() {
        // The instant, same-session model switch: a `SubmitUserInput` that
        // carries a pin RE-pins the session, so the next follow-up inherits the
        // LATEST pick (the switch sticks), a further re-pin updates it again, and
        // an unpinned follow-up never clobbers the current model. The repository
        // stays the session's originating one throughout.
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "repin-create").await;

        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::StartRun {
                        session_id: session,
                        objective: "diagnose".to_string(),
                        mode: AgentMode::Build,
                        repository: Some("/work/some-checkout".to_string()),
                        model: Some(ModelId("model-x".to_string())),
                    },
                    "repin-start",
                ),
            )
            .await
            .expect("start run");

        // First re-pick mid-conversation: switch to model-y.
        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::SubmitUserInput {
                        session_id: session,
                        text: "switch to Y".to_string(),
                        mode: AgentMode::Build,
                        model: Some(ModelId("model-y".to_string())),
                        envelope: None,
                    },
                    "repin-y",
                ),
            )
            .await
            .expect("submit input Y");

        let after_y = session_run_provenance(&pool, session)
            .await
            .expect("recover provenance");
        assert_eq!(
            after_y.model,
            Some(ModelId("model-y".to_string())),
            "a mid-conversation re-pin must stick for the next follow-up"
        );
        assert_eq!(
            after_y.repository.as_deref(),
            Some("/work/some-checkout"),
            "the repository stays the session's originating one"
        );

        // Second re-pick: switch again to model-z; the latest pick wins.
        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::SubmitUserInput {
                        session_id: session,
                        text: "now Z".to_string(),
                        mode: AgentMode::Build,
                        model: Some(ModelId("model-z".to_string())),
                        envelope: None,
                    },
                    "repin-z",
                ),
            )
            .await
            .expect("submit input Z");

        let after_z = session_run_provenance(&pool, session)
            .await
            .expect("recover provenance");
        assert_eq!(
            after_z.model,
            Some(ModelId("model-z".to_string())),
            "a second re-pin must update the session's current model"
        );

        // An unpinned follow-up must NOT clobber the current pin (inherits Z).
        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::SubmitUserInput {
                        session_id: session,
                        text: "keep going".to_string(),
                        mode: AgentMode::Build,
                        model: None,
                        envelope: None,
                    },
                    "repin-none",
                ),
            )
            .await
            .expect("submit input none");

        let after_none = session_run_provenance(&pool, session)
            .await
            .expect("recover provenance");
        assert_eq!(
            after_none.model,
            Some(ModelId("model-z".to_string())),
            "an unpinned follow-up inherits the session's current model, never clobbers it"
        );
    }

    #[tokio::test]
    async fn session_run_provenance_repin_switches_a_session_started_unpinned() {
        // The user-reported bug, at the daemon level: a session STARTED on the
        // default (unpinned) model, then the operator re-picks a model
        // mid-conversation. The re-pick must take effect for the next follow-up
        // instead of the session forever using the model it started with.
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "unpinned-create").await;

        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::StartRun {
                        session_id: session,
                        objective: "diagnose".to_string(),
                        mode: AgentMode::Build,
                        repository: None,
                        model: None,
                    },
                    "unpinned-start",
                ),
            )
            .await
            .expect("start run");

        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::SubmitUserInput {
                        session_id: session,
                        text: "use the big model".to_string(),
                        mode: AgentMode::Build,
                        model: Some(ModelId("just-picked".to_string())),
                        envelope: None,
                    },
                    "unpinned-repin",
                ),
            )
            .await
            .expect("submit input");

        let provenance = session_run_provenance(&pool, session)
            .await
            .expect("recover provenance");
        assert_eq!(
            provenance.model,
            Some(ModelId("just-picked".to_string())),
            "re-picking a model mid-conversation must switch the session, not be dropped"
        );
    }

    #[tokio::test]
    async fn session_run_provenance_defaults_without_a_start_run() {
        // A session with no applied `StartRun` (e.g. an unpinned older client
        // that sent no repository) yields the default, so a continuation falls
        // back exactly as before rather than misattributing a repository/model.
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "prov-empty-create").await;

        // An unpinned StartRun (no repository, no model) leaves nothing to
        // inherit — the recovered provenance is empty on both fields.
        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::StartRun {
                        session_id: session,
                        objective: "diagnose".to_string(),
                        mode: AgentMode::Build,
                        repository: None,
                        model: None,
                    },
                    "prov-empty-start",
                ),
            )
            .await
            .expect("start run");

        let provenance = session_run_provenance(&pool, session)
            .await
            .expect("recover provenance");
        assert_eq!(provenance.repository, None);
        assert_eq!(provenance.model, None);
    }

    #[tokio::test]
    async fn run_launch_provenance_recovers_a_continuations_repository_and_own_model() {
        // Recovery starts from the run that was left Queued/Running, not from
        // the live request body. A continuation therefore has to combine its
        // own model override with repository B from the session's StartRun;
        // falling back to daemon cwd (repository A) would execute in the wrong
        // checkout after restart.
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "launch-prov-create").await;
        let repository_b = tempdir().unwrap();
        let repository_b_string = repository_b.path().to_string_lossy().into_owned();

        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::StartRun {
                        session_id: session,
                        objective: "first".to_string(),
                        mode: AgentMode::Build,
                        repository: Some(repository_b_string.clone()),
                        model: Some(ModelId("original-model".to_string())),
                    },
                    "launch-prov-start",
                ),
            )
            .await
            .expect("start run");

        let continuation = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::SubmitUserInput {
                        session_id: session,
                        text: "continue".to_string(),
                        mode: AgentMode::Build,
                        model: Some(ModelId("override-model".to_string())),
                        envelope: None,
                    },
                    "launch-prov-input",
                ),
            )
            .await
            .expect("submit input")
            .created_run
            .expect("continuation run");

        let provenance = run_launch_provenance(&pool, continuation, session)
            .await
            .expect("recover launch provenance");
        assert_eq!(
            provenance.repository.as_deref(),
            Some(repository_b_string.as_str())
        );
        assert_eq!(
            provenance.model,
            Some(ModelId("override-model".to_string())),
            "the continuation's own model override must survive recovery"
        );
        assert_ne!(
            std::path::Path::new(provenance.repository.as_deref().unwrap()),
            std::env::current_dir().unwrap(),
            "recovery must not substitute the daemon cwd for repository B"
        );
    }

    #[tokio::test]
    async fn run_launch_provenance_ignores_commands_inserted_after_the_run_origin() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "launch-cutoff-create").await;

        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::StartRun {
                        session_id: session,
                        objective: "first".to_string(),
                        mode: AgentMode::Build,
                        repository: Some("/repository-at-origin".to_string()),
                        model: Some(ModelId("model-at-origin".to_string())),
                    },
                    "launch-cutoff-start",
                ),
            )
            .await
            .expect("start run");

        let continuation = processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::SubmitUserInput {
                        session_id: session,
                        text: "queued before a later repin".to_string(),
                        mode: AgentMode::Build,
                        model: None,
                        envelope: None,
                    },
                    "launch-cutoff-input",
                ),
            )
            .await
            .expect("submit input")
            .created_run
            .expect("continuation run");

        // Plant a later applied command directly. The provenance fold used to
        // scan newest-first without a cutoff, so this future command rewrote the
        // already-created continuation during restart recovery.
        let later = CommandBody::StartRun {
            session_id: session,
            objective: "later".to_string(),
            mode: AgentMode::Build,
            repository: Some("/repository-from-the-future".to_string()),
            model: Some(ModelId("future-model".to_string())),
        };
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO commands \
             (id, idempotency_key, session_id, client_id, body, status, result_json, received_at, applied_at) \
             VALUES (?, ?, ?, ?, ?, 'applied', '{}', ?, ?)",
        )
        .bind(CommandId::new().to_string())
        .bind("launch-cutoff-future")
        .bind(session.to_string())
        .bind(ClientId::new().to_string())
        .bind(serde_json::to_string(&later).unwrap())
        .bind(&now)
        .bind(&now)
        .execute(&pool)
        .await
        .expect("insert later repin");

        let provenance = run_launch_provenance(&pool, continuation, session)
            .await
            .expect("recover launch provenance at its cutoff");
        assert_eq!(
            provenance.repository.as_deref(),
            Some("/repository-at-origin")
        );
        assert_eq!(
            provenance.model,
            Some(ModelId("model-at-origin".to_string()))
        );
    }

    /// Adoption 20 regression: the composer's `#` quick-add is documented as
    /// "gated by the curator's secret and dedup filters", but the handler only
    /// appended a note — no memory was ever stored, so the feature persisted
    /// nothing. It now routes through `MemoryStore::curate`.
    #[tokio::test]
    async fn a_quick_add_memory_is_actually_stored_through_the_curator() {
        use codypendent_knowledge::{local_user_scope, MemoryStore};

        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "remember-create").await;

        processor
            .apply(
                &pool,
                ctx(ClientRole::Contributor),
                command(
                    CommandBody::RememberMemory {
                        session_id: session,
                        text: "the operator prefers conventional commit subjects".to_string(),
                    },
                    "remember-stored",
                ),
            )
            .await
            .expect("remember memory");

        let stored = MemoryStore::new()
            .query(&pool, &[local_user_scope()], None)
            .await
            .expect("query memories");
        assert_eq!(stored.len(), 1, "the quick-add is a durable memory");
        assert_eq!(
            stored[0].statement,
            "the operator prefers conventional commit subjects"
        );
        assert!(
            !stored[0].provenance.is_empty(),
            "the memory cites the note event it came from"
        );

        // The curator's dedup gate is now real: the same text again does not
        // add a second row.
        processor
            .apply(
                &pool,
                ctx(ClientRole::Contributor),
                command(
                    CommandBody::RememberMemory {
                        session_id: session,
                        text: "the operator prefers conventional commit subjects".to_string(),
                    },
                    "remember-duplicate",
                ),
            )
            .await
            .expect("remember memory again");
        let stored = MemoryStore::new()
            .query(&pool, &[local_user_scope()], None)
            .await
            .expect("query memories");
        assert_eq!(
            stored.len(),
            1,
            "a duplicate quick-add is deduped, not added"
        );
    }

    /// A pasted credential must never become durable — and the ledger is
    /// append-only, so the filter has to run BEFORE the note is written, not
    /// only inside `curate`. Nothing anywhere may echo the refused text.
    ///
    /// "Anywhere" includes the `commands` table (PR #68 review): the handler
    /// persists the command body for idempotency and the body carries the raw
    /// text, so a secret survived in the `body` column even though the ledger
    /// refused it. Both durable writes are checked here, and the redacted row
    /// must still replay to the same refusal.
    #[tokio::test]
    async fn a_secret_looking_quick_add_is_refused_without_reaching_the_ledger() {
        use codypendent_knowledge::{local_user_scope, MemoryStore};

        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "secret-create").await;

        let secret = "deploy with AKIAIOSFODNN7EXAMPLE";
        let first = processor
            .apply(
                &pool,
                ctx(ClientRole::Contributor),
                command(
                    CommandBody::RememberMemory {
                        session_id: session,
                        text: secret.to_string(),
                    },
                    "remember-secret",
                ),
            )
            .await
            .expect("the command still succeeds; the memory is refused");

        let stored = MemoryStore::new()
            .query(&pool, &[local_user_scope()], None)
            .await
            .expect("query memories");
        assert!(stored.is_empty(), "a secret never becomes a memory");

        let bodies: Vec<(String,)> =
            sqlx::query_as("SELECT body FROM events WHERE session_id = ? ORDER BY sequence")
                .bind(session.to_string())
                .fetch_all(&pool)
                .await
                .expect("read events");
        let ledger = bodies
            .iter()
            .map(|row| row.0.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !ledger.contains("AKIAIOSFODNN7EXAMPLE"),
            "the refused text never reaches the append-only ledger: {ledger}"
        );
        assert!(
            ledger.contains("refused by the secret filter"),
            "the operator is told the quick-add was refused: {ledger}"
        );

        // The `commands` row is durable too — same database, same backups.
        let commands: Vec<(String,)> = sqlx::query_as("SELECT body FROM commands")
            .fetch_all(&pool)
            .await
            .expect("read commands");
        let persisted = commands
            .iter()
            .map(|row| row.0.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !persisted.contains("AKIAIOSFODNN7EXAMPLE"),
            "the refused text never reaches the commands table either: {persisted}"
        );
        assert!(
            persisted.contains("refused by the secret filter"),
            "the row still records WHICH command arrived and why it was refused: {persisted}"
        );

        // …and the redacted row still replays to the same refusal outcome
        // (recorded result, no second note, no memory) rather than to a
        // different action.
        let replayed = processor
            .apply(
                &pool,
                ctx(ClientRole::Contributor),
                command(
                    CommandBody::RememberMemory {
                        session_id: session,
                        text: secret.to_string(),
                    },
                    "remember-secret",
                ),
            )
            .await
            .expect("replay of the refused quick-add");
        assert_eq!(replayed.command_id, first.command_id);
        assert_eq!(replayed.last_sequence, first.last_sequence);

        let after: Vec<(String,)> =
            sqlx::query_as("SELECT body FROM events WHERE session_id = ? ORDER BY sequence")
                .bind(session.to_string())
                .fetch_all(&pool)
                .await
                .expect("read events");
        assert_eq!(
            after.len(),
            bodies.len(),
            "the replay appended nothing new to the ledger"
        );
        assert!(
            MemoryStore::new()
                .query(&pool, &[local_user_scope()], None)
                .await
                .expect("query memories")
                .is_empty(),
            "the replay still stores no memory"
        );
    }

    /// `role_permits` ends in `_ => false`, so a NEW client-issued command that
    /// nobody remembers to list is silently role-denied and its whole feature is
    /// dead on the wire while every unit test of its handler still passes. That
    /// has now happened three times (`RunUserShell`, `RememberMemory`,
    /// `ReadArtifact`). This test pins the client-issued set: adding a command
    /// here without deciding its role floor fails loudly instead of shipping a
    /// feature that cannot be reached.
    #[test]
    fn every_client_issued_command_has_a_decided_role_floor() {
        let session = SessionId::new();
        // One representative of every command a CLIENT sends. Daemon-internal
        // bodies are deliberately absent.
        let client_issued: Vec<CommandBody> = vec![
            CommandBody::ReadArtifact {
                artifact_id: codypendent_protocol::ArtifactId::new(),
                offset: 0,
                limit: 1,
                expected_sha256: String::new(),
            },
            CommandBody::RunUserShell {
                session_id: session,
                command: "ls".to_string(),
            },
            CommandBody::RememberMemory {
                session_id: session,
                text: "a fact".to_string(),
            },
            CommandBody::RunEditorAction {
                session_id: session,
                action: codypendent_protocol::EditorNativeAction::ReviewCurrentFile,
                context: codypendent_protocol::EditorActionContext {
                    ide: codypendent_protocol::IdeContextUpdate::default(),
                    diagnostics: None,
                    repository_id: None,
                },
                model: None,
            },
            CommandBody::ExportBundle {
                request: codypendent_protocol::BundleExportRequest {
                    source_session_ids: vec![session],
                    inclusion: Default::default(),
                    redaction_policy: Default::default(),
                },
            },
            CommandBody::ListInbox {
                query: codypendent_protocol::InboxListQuery::default(),
            },
            // Session Library lifecycle: this one is NOT intercepted by the
            // connection layer, so it really does travel the generic write path
            // and a missing arm would make renaming/pinning/archiving/exporting
            // a session role-denied for every client.
            CommandBody::MutateSessionLifecycle {
                session_id: session,
                action: SessionLifecycleAction::Pin,
            },
            CommandBody::MutateInbox {
                mutation: codypendent_protocol::InboxMutation::Acknowledge {
                    entry_id: codypendent_protocol::InboxEntryId::new(),
                },
            },
            CommandBody::QueryAnalytics {
                query: codypendent_protocol::AnalyticsQuery::default(),
            },
            CommandBody::ExportAnalytics {
                request: codypendent_protocol::AnalyticsExportRequest {
                    query: codypendent_protocol::AnalyticsQuery::default(),
                    format: codypendent_protocol::AnalyticsExportFormat::Json,
                    max_rows: 100,
                },
            },
            CommandBody::ImportBundle {
                request: codypendent_protocol::BundleImportRequest {
                    bundle: codypendent_protocol::ArtifactRef {
                        id: codypendent_protocol::ArtifactId::new(),
                        media_type: "application/vnd.codypendent.bundle".into(),
                        byte_length: 1,
                        sha256: "ab".repeat(32),
                        sensitivity: codypendent_protocol::DataClassification::Confidential,
                    },
                    collision_policy: codypendent_protocol::bundle::BundleCollisionPolicy::Remap,
                },
            },
            CommandBody::ManageAutomationBinding {
                request: AutomationBindingRequest::Get {
                    id: codypendent_protocol::AutomationBindingId::new(),
                },
            },
            CommandBody::ManageAutomationBinding {
                request: AutomationBindingRequest::Create {
                    binding: codypendent_protocol::AutomationBindingDraft {
                        name: "test-binding".to_string(),
                        source: codypendent_protocol::TriggerSource::Manual,
                        workflow_id: codypendent_protocol::WorkflowId::new(),
                        workflow_version: "1".to_string(),
                        repository_id: codypendent_protocol::RepositoryId::new(),
                        filters: Default::default(),
                        invocation: Default::default(),
                        enabled: true,
                    },
                },
            },
            // Analytics budgets, listed for the same reason as the two
            // automation-binding rows above: a read variant and a mutating
            // variant, because a single-arm mistake could leave one of the two
            // groups permitted for no role at all.
            CommandBody::ManageAnalyticsBudget {
                request: AnalyticsBudgetRequest::Get {
                    id: "budget-1".to_string(),
                },
            },
            CommandBody::ManageAnalyticsBudget {
                request: AnalyticsBudgetRequest::Create {
                    budget: codypendent_protocol::AnalyticsBudgetDraft {
                        scope: codypendent_protocol::AnalyticsBudgetScope::Owner,
                        dimension: codypendent_protocol::AnalyticsBudgetDimension::CostMicros,
                        window: codypendent_protocol::AnalyticsBudgetWindow::Day,
                        threshold: 5_000,
                        enabled: true,
                    },
                },
            },
            CommandBody::MarketplaceSearch {
                query: "sample".to_string(),
                limit: None,
            },
            CommandBody::MarketplaceInstall {
                package_id: "sample".to_string(),
                manifest_toml: None,
                artifact_base64: None,
                allow_unsigned: false,
            },
            CommandBody::MarketplaceUpdate {
                package_id: "sample".to_string(),
                manifest_toml: None,
                artifact_base64: None,
                allow_unsigned: false,
            },
            CommandBody::MarketplaceEnable {
                package_id: "sample".to_string(),
                scope: None,
                session_id: None,
            },
            CommandBody::MarketplaceDisable {
                package_id: "sample".to_string(),
            },
            CommandBody::MarketplaceRevoke {
                package_id: "sample".to_string(),
                reason: "test".to_string(),
            },
            CommandBody::SecretDeclare {
                name: "token".to_string(),
                backend: "environment".to_string(),
                locator: "TOKEN".to_string(),
                capability: "api.read".to_string(),
                organization_id: None,
                repository_id: None,
            },
            CommandBody::SecretBind {
                reference_id: "ref_1".to_string(),
                job_id: "job_1".to_string(),
                capability: "api.read".to_string(),
            },
            CommandBody::SecretList { capability: None },
            CommandBody::SecretRevoke {
                reference_id: "ref_1".to_string(),
                reason: "test".to_string(),
            },
            // Milestone 6 federation & campaigns. All fourteen are intercepted
            // at the connection level, exactly as the marketplace and secret
            // bodies above are, and are listed for the same reason: a missing
            // arm makes the whole feature unreachable on the wire while every
            // handler unit test still passes.
            CommandBody::EstablishFederatedIdentity {
                repository: "/repo".to_string(),
                display_name: None,
            },
            CommandBody::GetPublicationPolicy {
                repository: "/repo".to_string(),
            },
            CommandBody::SetPublicationPolicy {
                repository: "/repo".to_string(),
                policy:
                    codypendent_protocol::federated_graph::UpdatePublicationPolicyRequest::default(),
            },
            CommandBody::PublishGraphFacts {
                repository: "/repo".to_string(),
                idempotency_key: "publish-1".to_string(),
            },
            CommandBody::TombstoneGraphFacts {
                repository: "/repo".to_string(),
                subject_kind: "node".to_string(),
                subject_id: "0".repeat(64),
                reason: "revoked".to_string(),
            },
            CommandBody::QueryFederatedGraph {
                query: codypendent_protocol::federated_graph::FederatedGraphQuery::default(),
            },
            CommandBody::QueryBlastRadius {
                query: codypendent_protocol::federated_graph::BlastRadiusQuery::default(),
            },
            CommandBody::PlanMigration {
                query: codypendent_protocol::federated_graph::MigrationPlanQuery {
                    source_repository: "/repo".to_string(),
                    source_symbol: "sample::symbol".to_string(),
                    target_symbol: None,
                    target_repositories: Vec::new(),
                    kind: codypendent_protocol::federated_graph::CampaignKind::ApiMigration,
                },
            },
            CommandBody::SuggestReviewers {
                query: codypendent_protocol::federated_graph::ReviewerSuggestionQuery::default(),
            },
            CommandBody::CreateCampaign {
                campaign: codypendent_protocol::federated_graph::CreateCampaignRequest {
                    title: "migrate".to_string(),
                    kind: codypendent_protocol::federated_graph::CampaignKind::ApiMigration,
                    workflow_id: "fix-ci".to_string(),
                    repositories: Vec::new(),
                    idempotency_key: "campaign-1".to_string(),
                },
            },
            CommandBody::GetCampaign {
                campaign_id: "campaign-1".to_string(),
            },
            CommandBody::ListCampaigns {
                state: None,
                limit: None,
            },
            CommandBody::ExecuteCampaign {
                request: codypendent_protocol::federated_graph::ExecuteCampaignRequest {
                    campaign_id: "campaign-1".to_string(),
                    retry_failed_only: false,
                },
            },
            CommandBody::CancelCampaign {
                campaign_id: "campaign-1".to_string(),
            },
        ];
        for body in &client_issued {
            let reachable = [
                ClientRole::Observer,
                ClientRole::Contributor,
                ClientRole::Approver,
                ClientRole::Controller,
            ]
            .iter()
            .any(|role| role_permits(*role, body));
            assert!(
                reachable,
                "{body:?} is permitted for NO role — it fell through `_ => false`, \
                 so the feature is unreachable on the wire. Give it a role floor."
            );
        }
    }

    #[test]
    fn composer_actions_are_role_gated_like_submit_input() {
        // Adoption 20 regression: `!` shell escape and `#` memory quick-add are
        // new command bodies. They must be listed in `role_permits` or the
        // authorization gate rejects them with `protocol.role-denied` before the
        // handler ever runs — which would silently kill the composer features
        // even though the handler unit-tests (which bypass the gate) pass.
        let shell = CommandBody::RunUserShell {
            session_id: SessionId::new(),
            command: "ls".to_string(),
        };
        let remember = CommandBody::RememberMemory {
            session_id: SessionId::new(),
            text: "a fact".to_string(),
        };
        for body in [&shell, &remember] {
            assert!(
                role_permits(ClientRole::Contributor, body),
                "contributor must be allowed to issue {body:?}"
            );
            assert!(
                role_permits(ClientRole::Controller, body),
                "controller must be allowed to issue {body:?}"
            );
            assert!(
                !role_permits(ClientRole::Observer, body),
                "observer must be denied {body:?}"
            );
        }
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
                        model: None,
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
                        model: None,
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
                        model: None,
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
                        model: None,
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
                        model: None,
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
                        model: None,
                        envelope: None,
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
            pending_approvals: Vec::new(),
            pending_prompts: Vec::new(),
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
    async fn reserved_command_is_rejected_before_role_checks() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();

        let err = processor
            .apply(
                &pool,
                ctx(ClientRole::Observer),
                command(
                    CommandBody::MutateSessionLifecycle {
                        session_id: SessionId::new(),
                        action: codypendent_protocol::SessionLifecycleAction::Unknown,
                    },
                    "reserved-query",
                ),
            )
            .await
            .expect_err("reserved body rejected");
        assert_eq!(err.code, "protocol.unsupported-payload");
        assert!(!err.retryable);
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
                        repository: None,
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
        let questions = QuestionBroker::new();
        let processor =
            CommandProcessor::new(SubscriptionHub::new(), broker.clone(), questions.clone());
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
                        model: None,
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
                None,
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
        let questions = QuestionBroker::new();
        let processor =
            CommandProcessor::new(SubscriptionHub::new(), broker.clone(), questions.clone());
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
                        model: None,
                    },
                    "start",
                ),
            )
            .await
            .unwrap()
            .created_run
            .unwrap();

        let a1 = broker
            .request(
                &pool,
                session,
                run,
                None,
                ProposedAction::ExecuteCommand {
                    program: "cargo".to_string(),
                    args: vec!["test".to_string()],
                    environment: Vec::new(),
                    cwd: None,
                },
                Risk {
                    level: RiskLevel::Low,
                    reasons: vec![],
                },
                vec![],
                None,
            )
            .await
            .unwrap();
        let a2 = broker
            .request(
                &pool,
                session,
                run,
                None,
                ProposedAction::ExecuteCommand {
                    program: "cargo".to_string(),
                    args: vec!["bench".to_string()],
                    environment: Vec::new(),
                    cwd: None,
                },
                Risk {
                    level: RiskLevel::Low,
                    reasons: vec![],
                },
                vec![],
                None,
            )
            .await
            .unwrap();

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

    #[tokio::test]
    async fn resolve_question_delegates_to_broker_and_bumps_revision() {
        use codypendent_protocol::{QuestionOption, QuestionPrompt};

        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let broker = ApprovalBroker::new();
        let questions = QuestionBroker::new();
        let processor =
            CommandProcessor::new(SubscriptionHub::new(), broker.clone(), questions.clone());
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
                        model: None,
                    },
                    "start",
                ),
            )
            .await
            .unwrap()
            .created_run
            .unwrap();

        let prompt = QuestionPrompt {
            question: "Confirm?".to_string(),
            header: "Confirm".to_string(),
            options: vec![QuestionOption {
                label: "Yes".to_string(),
                description: String::new(),
            }],
            multiple: false,
            custom: true,
        };

        let question_id = questions
            .ask(&pool, session, run, vec![prompt])
            .await
            .unwrap();

        let mut rx = processor.subscriptions().subscribe(session);

        let resolve_cmd = command(
            CommandBody::ResolveQuestion {
                question_id,
                outcome: QuestionOutcome::Answered {
                    answers: vec![vec!["Yes".to_string()]],
                },
            },
            "resolve-q",
        );

        let outcome = processor
            .apply(&pool, ctx(ClientRole::Approver), resolve_cmd)
            .await
            .expect("resolve question applies");

        assert!(outcome.last_sequence.is_some());

        let (state,): (String,) = sqlx::query_as("SELECT state FROM questions WHERE id = ?")
            .bind(question_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "answered");

        let event = rx.recv().await.unwrap();
        assert!(matches!(
            event.body,
            EventBody::QuestionResolved {
                outcome: QuestionOutcome::Answered { .. },
                ..
            }
        ));
    }

    #[tokio::test]
    async fn queue_prompt_appends_one_snapshot_event_in_the_command_transaction() {
        let tmp = tempdir().unwrap();
        let pool = test_pool(tmp.path()).await;
        let hub = SubscriptionHub::new();
        let broker = ApprovalBroker::new();
        let qbroker = QuestionBroker::new();
        let proc = CommandProcessor::new(hub, broker, qbroker);

        let session_id = SessionId::new();
        crate::ledger::create_session(&pool, session_id, "Test Session")
            .await
            .unwrap();

        let queue_cmd = Command {
            command_id: CommandId::new(),
            idempotency_key: "k-queue-1".to_string(),
            expected_revision: None,
            body: CommandBody::QueuePrompt {
                session_id,
                text: "queued message".to_string(),
                mode: AgentMode::Build,
                delivery: PromptDelivery::Queue,
            },
        };

        let outcome = proc
            .apply(&pool, ctx(ClientRole::Contributor), queue_cmd.clone())
            .await
            .unwrap();

        assert!(outcome.newly_applied);
        let events = crate::ledger::load_events(&pool, session_id).await.unwrap();
        assert_eq!(events.len(), 1); // PendingPromptsChanged
        let ev = &events[0];
        match &ev.body {
            EventBody::PendingPromptsChanged { prompts } => {
                assert_eq!(prompts.len(), 1);
                assert_eq!(prompts[0].text, "queued message");
                assert_eq!(prompts[0].delivery, PromptDelivery::Queue);
            }
            other => panic!("expected PendingPromptsChanged, got {other:?}"),
        }

        // Duplicate delivery produces one result (idempotency replay)
        let outcome_dup = proc
            .apply(&pool, ctx(ClientRole::Contributor), queue_cmd)
            .await
            .unwrap();
        assert!(!outcome_dup.newly_applied);
        let events_after_dup = crate::ledger::load_events(&pool, session_id).await.unwrap();
        assert_eq!(events_after_dup.len(), 1);
    }

    #[tokio::test]
    async fn queue_steering_enqueues_steer_entry_and_journals_steering_queued() {
        let tmp = tempdir().unwrap();
        let pool = test_pool(tmp.path()).await;
        let hub = SubscriptionHub::new();
        let broker = ApprovalBroker::new();
        let qbroker = QuestionBroker::new();
        let proc = CommandProcessor::new(hub, broker, qbroker);

        let session_id = SessionId::new();
        crate::ledger::create_session(&pool, session_id, "Test Session")
            .await
            .unwrap();

        // Start a run
        let start_cmd = Command {
            command_id: CommandId::new(),
            idempotency_key: "k-start-1".to_string(),
            expected_revision: None,
            body: CommandBody::StartRun {
                session_id,
                objective: "build app".to_string(),
                mode: AgentMode::Build,
                repository: None,
                model: None,
            },
        };
        let start_outcome = proc
            .apply(&pool, ctx(ClientRole::Contributor), start_cmd)
            .await
            .unwrap();
        let started_run_id = start_outcome.created_run.unwrap();

        // Queue steering text
        let steer_cmd = Command {
            command_id: CommandId::new(),
            idempotency_key: "k-steer-1".to_string(),
            expected_revision: None,
            body: CommandBody::QueueSteering {
                run_id: started_run_id,
                text: "steer live run".to_string(),
            },
        };

        let outcome = proc
            .apply(&pool, ctx(ClientRole::Contributor), steer_cmd)
            .await
            .unwrap();
        assert!(outcome.newly_applied);

        let events = crate::ledger::load_events(&pool, session_id).await.unwrap();
        // Events: RunStarted, SteeringQueued, PendingPromptsChanged
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[1].body,
            EventBody::SteeringQueued { run_id: r } if r == started_run_id
        ));
        match &events[2].body {
            EventBody::PendingPromptsChanged { prompts } => {
                assert_eq!(prompts.len(), 1);
                assert_eq!(prompts[0].text, "steer live run");
                assert_eq!(prompts[0].delivery, PromptDelivery::Steer);
            }
            other => panic!("expected PendingPromptsChanged, got {other:?}"),
        }

        // Verify prompt was persisted in pending_prompts table
        let snap = crate::prompt_queue::snapshot_pool(&pool, session_id)
            .await
            .unwrap();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].text, "steer live run");
        assert_eq!(snap[0].delivery, PromptDelivery::Steer);
    }

    // --- FIX 2: `!` operator shell escape (now SANDBOXED) ---

    /// Whether this host has an enforcing OS sandbox (Seatbelt on macOS, bwrap
    /// on Linux). Real-confinement assertions gate on this the way the sandbox
    /// enforcement suite does, so CI on macos-seatbelt and linux-bwrap runs them
    /// while a host lacking the tool skips them instead of spuriously failing.
    fn sandbox_available() -> bool {
        enforcing_executor()
            .map(|executor| executor.capability_report().available)
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn user_shell_runs_confined_in_the_session_repository() {
        // (b)+(sandbox) The `!` command runs CONFINED in the session's
        // repository (its `StartRun.repository`), NOT the daemon's frozen cwd.
        // A `touch` lands its marker in the repo dir iff the confined cwd was
        // the granted worktree — proving both the cwd fix and that confinement
        // permits worktree writes.
        if !sandbox_available() {
            eprintln!("skipping: no enforcing OS sandbox on this host");
            return;
        }
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "sh-cwd-create").await;

        let repo = tempdir().unwrap();
        processor
            .apply(
                &pool,
                ctx(ClientRole::Controller),
                command(
                    CommandBody::StartRun {
                        session_id: session,
                        objective: "go".to_string(),
                        mode: AgentMode::Build,
                        repository: Some(repo.path().to_string_lossy().to_string()),
                        model: None,
                    },
                    "sh-cwd-start",
                ),
            )
            .await
            .expect("start run");

        // Drive the handler directly (mirroring the module's write-path unit
        // tests): this exercises the restructured claim → confined exec → apply
        // flow and the session-repository cwd resolution without depending on
        // the wire role gate.
        let shell_cmd = command(
            CommandBody::RunUserShell {
                session_id: session,
                command: "touch cwd_marker".to_string(),
            },
            "sh-cwd-run",
        );
        processor
            .apply_run_user_shell(
                &pool,
                &ctx(ClientRole::Controller),
                &shell_cmd,
                session,
                "touch cwd_marker".to_string(),
            )
            .await
            .expect("run user shell");

        assert!(
            repo.path().join("cwd_marker").exists(),
            "the confined `!` command did not run in (or could not write) the session repository"
        );
    }

    #[tokio::test]
    async fn user_shell_fails_closed_and_finalizes_its_claim_on_invalid_provenance() {
        // An unreadable provenance ledger must never turn into "run in daemon
        // cwd". Plant a malformed applied StartRun command, then use an
        // absolute marker so any accidental execution is observable even on a
        // host without an enforcing sandbox. The shell command's durable claim
        // still has to become `applied`, otherwise replay could execute it.
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "sh-invalid-prov-create").await;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO commands \
             (id, idempotency_key, session_id, client_id, body, status, result_json, received_at, applied_at) \
             VALUES (?, ?, ?, ?, ?, 'applied', '{}', ?, ?)",
        )
        .bind(CommandId::new().to_string())
        .bind("malformed-start-provenance")
        .bind(session.to_string())
        .bind(ClientId::new().to_string())
        .bind("{\"type\":\"StartRun\"")
        .bind(&now)
        .bind(&now)
        .execute(&pool)
        .await
        .expect("plant malformed provenance");

        let marker = dir.path().join("must-not-exist");
        let shell = format!("touch {}", marker.to_string_lossy());
        processor
            .apply(
                &pool,
                ctx(ClientRole::Contributor),
                command(
                    CommandBody::RunUserShell {
                        session_id: session,
                        command: shell,
                    },
                    "sh-invalid-prov-run",
                ),
            )
            .await
            .expect("provenance refusal is durably finalized");

        assert!(!marker.exists(), "the shell command must not execute");
        let status: String =
            sqlx::query_scalar("SELECT status FROM commands WHERE idempotency_key = ?")
                .bind("sh-invalid-prov-run")
                .fetch_one(&pool)
                .await
                .expect("load shell command status");
        assert_eq!(status, "applied", "the non-idempotent claim is finalized");
        let events = crate::ledger::load_events(&pool, session).await.unwrap();
        assert!(events.iter().any(|event| matches!(
            &event.body,
            EventBody::NoteAppended { text, .. }
                if text.contains("was not run") && text.contains("session repository")
        )));
    }

    #[tokio::test]
    async fn mid_flight_user_shell_is_not_re_executed_on_resume() {
        // (a)+(e) A `!` command is a non-idempotent external effect. A crash
        // mid-exec leaves a committed `received` row; the resume path must
        // finalize it applied WITHOUT re-running the command. Simulate the
        // crash by planting the `received` row, then re-delivering the same
        // idempotency key: the marker must NOT be created.
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let processor = CommandProcessor::default();
        let session = create_session(&processor, &pool, "sh-resume-create").await;
        let repo = tempdir().unwrap();

        let marker = repo.path().join("resume_marker");
        let shell = format!("touch {}", marker.to_string_lossy());
        let body = CommandBody::RunUserShell {
            session_id: session,
            command: shell.clone(),
        };
        let key = "sh-resume-run";

        // Plant a `received` row exactly as the claim step would have committed
        // it before the crash.
        sqlx::query(
            "INSERT INTO commands \
             (id, idempotency_key, session_id, client_id, body, status, received_at) \
             VALUES (?, ?, ?, ?, ?, 'received', ?)",
        )
        .bind(CommandId::new().to_string())
        .bind(key)
        .bind(session.to_string())
        .bind(ClientId::new().to_string())
        .bind(serde_json::to_string(&body).unwrap())
        .bind(Utc::now().to_rfc3339())
        .execute(&pool)
        .await
        .expect("plant received row");

        let outcome = processor
            .apply(&pool, ctx(ClientRole::Contributor), command(body, key))
            .await
            .expect("resume applies");

        assert!(
            !outcome.newly_applied,
            "a resumed `!` command is a replay, not a fresh application"
        );
        assert!(
            !marker.exists(),
            "the mid-flight `!` command was wrongly re-executed on resume"
        );
    }

    /// Build a synthetic [`SandboxOutcome`], mirroring the hook-exec tests, so
    /// the note mapping is exercised deterministically on any platform.
    fn shell_outcome(
        exit: Option<i32>,
        timed_out: bool,
        stdout: &str,
        stderr: &str,
        truncated: bool,
    ) -> SandboxOutcome {
        SandboxOutcome {
            backend: codypendent_sandbox::executor::SandboxBackend::None,
            exit_code: exit,
            timed_out,
            duration: std::time::Duration::from_millis(5),
            stdout: codypendent_sandbox::sanitize::sanitize_untrusted("shell:test", stdout, 4096),
            stderr: codypendent_sandbox::sanitize::sanitize_untrusted("shell:test", stderr, 4096),
            output_truncated: truncated,
        }
    }

    #[test]
    fn user_shell_note_reports_a_timeout() {
        // (c) A hung command hits the sandbox wall-clock cap; the note says so
        // rather than surfacing partial output as if the command completed.
        let note = user_shell_note(Ok(shell_outcome(None, true, "partial", "", false)));
        assert!(
            note.contains("timed out"),
            "a timed-out command must report it, got {note:?}"
        );
    }

    #[test]
    fn user_shell_note_marks_truncated_output() {
        // (d) The sandbox caps output (`maximum_output_mb`); when it truncates,
        // the note is annotated so the operator knows the tail was dropped.
        let note = user_shell_note(Ok(shell_outcome(
            Some(0),
            false,
            "lots of output",
            "",
            true,
        )));
        assert!(
            note.contains("truncated"),
            "truncated output must be annotated, got {note:?}"
        );
    }

    #[test]
    fn user_shell_note_combines_stdout_and_stderr() {
        let note = user_shell_note(Ok(shell_outcome(Some(0), false, "out", "err", false)));
        assert!(note.contains("out") && note.contains("err"), "got {note:?}");
    }

    #[tokio::test]
    async fn user_shell_is_refused_fail_closed_when_sandbox_unavailable() {
        // Fail-closed identity: a platform with no backend, or a missing tool,
        // REFUSES the `!` command with a legible note — never runs it unconfined.
        let unsupported = shell_escape_refusal(&SandboxError::UnsupportedPlatform {
            platform: "windows",
        });
        assert!(unsupported.contains("shell escape refused"));
        assert!(unsupported.contains("windows"));

        let missing_tool = user_shell_note(Err(SandboxError::ToolUnavailable {
            tool: "bwrap".to_string(),
            diagnostic: "not found on PATH".to_string(),
        }));
        assert!(missing_tool.contains("shell escape refused"));
        assert!(missing_tool.contains("bwrap"));

        // On a host that genuinely lacks the sandbox, the end-to-end handler
        // refuses too (only runs where the tool is actually absent).
        if !sandbox_available() {
            let note = run_user_shell_command(
                "touch should_not_exist",
                std::path::Path::new("."),
                "shell:test",
            )
            .await;
            assert!(
                note.contains("shell escape refused"),
                "an unavailable sandbox must refuse, got {note:?}"
            );
            assert!(
                !std::path::Path::new("should_not_exist").exists(),
                "a refused `!` command must not have executed"
            );
        }
    }
}
