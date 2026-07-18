//! The framework agent loop (STEP 1.10).
//!
//! [`FrameworkAgentRuntime`] drives the Chapter 04 Level-1 deterministic
//! workflow — `Inspect → Plan → Modify → Test → Review → Present` — around a
//! [`ModelDriver`], layering the daemon's durable semantics on top: persisted
//! run-state transitions, policy + approval middleware for every tool the model
//! proposes, artifact/observation compaction, modes, cancellation, safe-point
//! steering, a change-set at the review node, and a run chronicle at the
//! terminal state. The daemon (this loop) is the *only* component that executes
//! tools (invariant 2); a client disconnect has zero effect because the loop
//! holds no client handles — it only publishes to a [`SubscriptionHub`], and
//! publishing to zero subscribers is normal.
//!
//! ## The model is decoupled from the loop
//!
//! The loop never talks to an LLM directly. It asks a [`ModelDriver`] for the
//! next [`ModelStep`] given the transcript so far, which makes the whole loop
//! deterministically testable with a [`ScriptedDriver`] — no live model, no
//! HTTP. The [`FrameworkModelDriver`] (behind `provider-openai`) wraps a real
//! `agent_framework_openai::OpenAIChatCompletionClient`.
//!
//! ## The SQLite boundary (why a [`RunJournal`] and [`ArtifactSink`], not a pool)
//!
//! `sqlx` is not a dependency of this crate (ADR-009; the tool layer explains
//! this at length — see [`crate::tools`]). So this module cannot name
//! `SqlitePool`, cannot open a transaction, and cannot call the daemon's
//! pool-taking helpers directly. Exactly as the tools reach the artifact store
//! through the pool-erased [`ArtifactSink`]/[`ClosureSink`] boundary, the loop
//! reaches the ledger, run projection, and approval broker through a
//! [`RunJournal`] built from closures that capture a pool *value* whose type is
//! only ever inferred (never named). The daemon-side caller (STEP 1.11) — and
//! the integration tests — construct the journal and sink where the pool is in
//! scope; the loop stays pure orchestration.
//!
//! [`SubscriptionHub`]: codypendent_daemon::subscriptions::SubscriptionHub
//! [`ArtifactSink`]: crate::tools::ArtifactSink
//! [`ClosureSink`]: crate::tools::ClosureSink

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use codypendent_daemon::approvals::ApprovalBroker;
use codypendent_daemon::artifacts::Provenance;
use codypendent_daemon::policy::{
    Capability, Decision, EvalContext, ModeOverlay, PathScope, PolicyEngine,
};
use codypendent_daemon::subscriptions::SubscriptionHub;
use codypendent_protocol::{
    Actor, AgentId, AgentMode, ApprovalDecision, ApprovalId, ArtifactId, ArtifactRef,
    BudgetDimension, ChangeSetId, EventBody, ModelId, ProposedAction, Risk, RiskLevel,
    RunDisposition, RunId, RunState, SessionEvent, SessionId, ToolOutcome,
};

use codypendent_integrations::github::{GitHubApi, GitHubError, RepoId};
use codypendent_integrations::ide::digest_bytes;
use codypendent_protocol::ide::{DirtyBufferDigest, SourceProvenance};

use crate::models::ModelRegistry;
use crate::tools::{
    new_pull_request, parse_create_check_run, parse_create_draft_pull_request,
    parse_get_pull_request, parse_list_check_runs, parse_update_pull_request, render_check_runs,
    render_pull_request, ApplyPatch, ApplyPatchInput, ArtifactSink, CommandRequest,
    CreateCheckRunInput, CreateCheckRunSummary, CreateDraftPullRequest,
    CreateDraftPullRequestInput, EnvironmentBinding, GetPullRequest, GetPullRequestInput, GitDiff,
    GitDiffInput, ListCheckRuns, ListCheckRunsInput, ReadFile, ReadFileInput, Search, SearchInput,
    Shell, UpdatePullRequestInput, UpdatePullRequestTool,
};

/// Safety valve: the maximum number of `next_step` calls a single run makes
/// before the loop gives up. A well-behaved driver returns [`ModelStep::Finish`];
/// this bounds a pathological or buggy one.
const MAX_STEPS: usize = 256;

/// Safety valve: the wall-clock ceiling for a single run. `MAX_STEPS` bounds how
/// many model requests are made, not how long each (or its tools) takes; this
/// bounds the total. A `BudgetWarning { WallClock }` is emitted at 80%.
const MAX_WALL_CLOCK_SECS: u64 = 30 * 60;

/// Default wall-clock timeout for a model-proposed `shell.run` when the model
/// does not specify one (further clamped down by the command scope).
const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// Transcript, steps, and the ModelDriver trait
// ---------------------------------------------------------------------------

/// One entry in the conversation the loop maintains and hands to the driver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TurnItem {
    /// The run objective, seeded as the first item.
    Objective(String),
    /// Model-authored natural-language text.
    Assistant(String),
    /// The observation fed back after a tool call (already compacted).
    ToolResult {
        /// The tool that produced the observation.
        tool: String,
        /// The compacted, model-facing output.
        output: String,
    },
    /// User steering text injected at a safe point.
    Steering(String),
}

/// The next thing the model wants to do, as decided by a [`ModelDriver`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ModelStep {
    /// Emit natural-language text (streamed to clients as a delta).
    Say(String),
    /// Call a tool with JSON arguments.
    CallTool {
        /// The tool name, e.g. `shell.run`.
        tool: String,
        /// The tool arguments as JSON.
        args: Value,
    },
    /// Conclude the run with a short summary.
    Finish {
        /// A human-readable summary of the run.
        summary: String,
    },
}

/// Produces the next [`ModelStep`] from the conversation so far. The loop is
/// written entirely against this trait, so it runs identically with a scripted
/// driver (tests) or a live framework client.
#[async_trait]
pub trait ModelDriver: Send + Sync {
    /// The model id this driver represents, recorded in run attribution and
    /// per-request trace metadata.
    fn model_id(&self) -> ModelId;

    /// Given the conversation so far, produce the next step.
    async fn next_step(&self, transcript: &[TurnItem]) -> anyhow::Result<ModelStep>;
}

/// A driver backed by a fixed queue of pre-set steps — the deterministic engine
/// under the loop's tests. Once the queue drains it returns
/// [`ModelStep::Finish`], so a loop can never hang on an exhausted script.
pub struct ScriptedDriver {
    steps: Mutex<std::collections::VecDeque<ModelStep>>,
    model_id: ModelId,
}

impl ScriptedDriver {
    /// A scripted driver that yields `steps` in order.
    pub fn new(steps: Vec<ModelStep>) -> Self {
        Self {
            steps: Mutex::new(steps.into_iter().collect()),
            model_id: ModelId("scripted".to_string()),
        }
    }

    /// Set the reported model id (defaults to `scripted`).
    pub fn with_model(mut self, model_id: ModelId) -> Self {
        self.model_id = model_id;
        self
    }
}

#[async_trait]
impl ModelDriver for ScriptedDriver {
    fn model_id(&self) -> ModelId {
        self.model_id.clone()
    }

    async fn next_step(&self, _transcript: &[TurnItem]) -> anyhow::Result<ModelStep> {
        let mut queue = self.steps.lock().expect("scripted driver mutex poisoned");
        Ok(queue.pop_front().unwrap_or(ModelStep::Finish {
            summary: "scripted run complete".to_string(),
        }))
    }
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

/// A cancellation token built over a `tokio::sync::watch` (`tokio_util` is not a
/// dependency). Cheap to clone; a single [`CancellationHandle::cancel`] flips
/// every clone.
#[derive(Debug, Clone)]
pub struct CancellationToken {
    rx: tokio::sync::watch::Receiver<bool>,
}

impl CancellationToken {
    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Resolve once cancellation has been requested — immediately if it already
    /// has. Cancellation-safe, so it can race another future inside a
    /// `tokio::select!`. If the controlling handle is dropped without ever
    /// cancelling, this parks forever (letting the other `select!` arm win).
    pub async fn cancelled(&self) {
        let mut rx = self.rx.clone();
        if *rx.borrow() {
            return;
        }
        while rx.changed().await.is_ok() {
            if *rx.borrow() {
                return;
            }
        }
        // The sender was dropped without a cancel: never fires.
        std::future::pending::<()>().await
    }

    /// A token that is never cancelled (its source is dropped, so the retained
    /// value stays `false`). Convenient for runs that opt out of cancellation.
    pub fn never() -> Self {
        cancellation().1
    }
}

/// The controlling side of a [`CancellationToken`]. Holding it keeps the channel
/// alive; calling [`cancel`](CancellationHandle::cancel) requests cancellation.
#[derive(Debug)]
pub struct CancellationHandle {
    tx: tokio::sync::watch::Sender<bool>,
}

impl CancellationHandle {
    /// Request cancellation. Idempotent.
    pub fn cancel(&self) {
        let _ = self.tx.send(true);
    }
}

/// Create a linked ([`CancellationHandle`], [`CancellationToken`]) pair.
pub fn cancellation() -> (CancellationHandle, CancellationToken) {
    let (tx, rx) = tokio::sync::watch::channel(false);
    (CancellationHandle { tx }, CancellationToken { rx })
}

// ---------------------------------------------------------------------------
// Run context, modes
// ---------------------------------------------------------------------------

/// Everything the loop needs to know about the run it is executing. The `runs`
/// row (created by the STEP 1.3 command pipeline) already exists; this is the
/// in-memory execution context.
pub struct RunContext {
    /// The owning session (the ledger the run appends to).
    pub session_id: SessionId,
    /// This run's id.
    pub run_id: RunId,
    /// The objective, seeded as the first transcript item.
    pub objective: String,
    /// The mode preset, mapped to a [`ModeOverlay`] for policy enforcement.
    pub mode: AgentMode,
    /// The repository root (`$REPOSITORY`).
    pub repository: PathBuf,
    /// The run's writable worktree (`$WORKTREE`).
    pub worktree: PathBuf,
    /// The GitHub repository this run targets (`owner/repo`), if GitHub is
    /// configured. The client handle lives on the runtime; this names the target.
    pub github_repo: Option<RepoId>,
    /// Digests of the IDE's unsaved ("dirty") buffers at run start (Phase 3 STEP
    /// 3.4). The read path labels an excerpt whose on-disk bytes diverge from one
    /// of these as `unsaved-ide-buffer`, so the trace flags possibly-stale reads.
    pub ide_dirty_buffers: Vec<DirtyBufferDigest>,
    /// Optional channel of queued steering text, drained at safe points.
    pub steering: Option<mpsc::UnboundedReceiver<String>>,
}

impl RunContext {
    /// A context with no steering channel.
    pub fn new(
        session_id: SessionId,
        run_id: RunId,
        objective: impl Into<String>,
        mode: AgentMode,
        repository: impl Into<PathBuf>,
        worktree: impl Into<PathBuf>,
    ) -> Self {
        Self {
            session_id,
            run_id,
            objective: objective.into(),
            mode,
            repository: repository.into(),
            worktree: worktree.into(),
            github_repo: None,
            ide_dirty_buffers: Vec::new(),
            steering: None,
        }
    }

    /// Attach a steering channel.
    pub fn with_steering(mut self, steering: mpsc::UnboundedReceiver<String>) -> Self {
        self.steering = Some(steering);
        self
    }

    /// Name the GitHub repository this run targets, enabling the `github.*`
    /// tools (the client handle is injected on the runtime separately).
    pub fn with_github_repo(mut self, repo: RepoId) -> Self {
        self.github_repo = Some(repo);
        self
    }

    /// Seed the run with the IDE's unsaved-buffer digests (Phase 3 STEP 3.4), so
    /// the read path can label a read whose disk bytes diverge from an editor
    /// buffer as `unsaved-ide-buffer`.
    pub fn with_ide_context(mut self, dirty_buffers: Vec<DirtyBufferDigest>) -> Self {
        self.ide_dirty_buffers = dirty_buffers;
        self
    }
}

/// Map an [`AgentMode`] to the policy [`ModeOverlay`] that enforces it. The
/// overlay only ever *further restricts* the file policy (an overlay can never
/// widen a security restriction), so an `Explore` run proposing a write is
/// denied by policy regardless of what the model says.
pub fn mode_overlay(mode: AgentMode) -> ModeOverlay {
    match mode {
        // Ask/Explore are read-only: writes and commands denied.
        AgentMode::Ask | AgentMode::Explore => ModeOverlay::read_only(),
        // Plan may run safe probes but writes only plan artifacts (never the
        // worktree), so worktree writes are denied.
        AgentMode::Plan => ModeOverlay {
            write_allowed: false,
            command_allowed: true,
            network_allowed: false,
        },
        // Build gets the full worktree write scope (still gated by the file
        // policy and per-command approval).
        AgentMode::Build => ModeOverlay::permissive(),
        // Review is read + comment: read-only verification, no writes.
        AgentMode::Review => ModeOverlay {
            write_allowed: false,
            command_allowed: true,
            network_allowed: false,
        },
        // An unknown/future mode collapses to the most restrictive overlay.
        _ => ModeOverlay::read_only(),
    }
}

// ---------------------------------------------------------------------------
// The RunJournal: pool-erased persistence, mirroring the ArtifactSink boundary
// ---------------------------------------------------------------------------

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// The arguments an approval request carries into the [`ApprovalBroker`].
pub struct ApprovalRequest {
    /// The session whose ledger records the request.
    pub session_id: SessionId,
    /// The run proposing the action.
    pub run_id: RunId,
    /// The action awaiting approval.
    pub action: ProposedAction,
    /// The risk assessment shown to the approver.
    pub risk: Risk,
    /// The capabilities the grant would mint.
    pub capabilities: Vec<Capability>,
}

/// Pool-erased persistence for the loop.
///
/// Built from two closures that capture a `SqlitePool` value (whose type this
/// crate cannot name — see the module docs). `persist` records one event
/// (allocating its sequence, and — when the body is
/// [`EventBody::RunStateChanged`] — updating the `runs` row in step) and returns
/// the persisted [`SessionEvent`] so the loop can publish it. `request_approval`
/// drives [`ApprovalBroker::request`], which itself persists `ApprovalRequested`
/// and returns the new [`ApprovalId`].
///
/// The `request_approval` closure MUST drive the *same* [`ApprovalBroker`]
/// instance (a clone) held by the runtime, so that
/// [`ApprovalBroker::await_decision`] on the runtime observes the resolution.
pub struct RunJournal {
    persist: Box<
        dyn Fn(SessionId, Actor, EventBody) -> BoxFuture<anyhow::Result<SessionEvent>>
            + Send
            + Sync,
    >,
    request_approval:
        Box<dyn Fn(ApprovalRequest) -> BoxFuture<anyhow::Result<ApprovalId>> + Send + Sync>,
}

impl RunJournal {
    /// Build a journal from a persist closure and an approval-request closure.
    pub fn new<PF, PFut, AF, AFut>(persist: PF, request_approval: AF) -> Self
    where
        PF: Fn(SessionId, Actor, EventBody) -> PFut + Send + Sync + 'static,
        PFut: Future<Output = anyhow::Result<SessionEvent>> + Send + 'static,
        AF: Fn(ApprovalRequest) -> AFut + Send + Sync + 'static,
        AFut: Future<Output = anyhow::Result<ApprovalId>> + Send + 'static,
    {
        Self {
            persist: Box::new(move |session, actor, body| Box::pin(persist(session, actor, body))),
            request_approval: Box::new(move |req| Box::pin(request_approval(req))),
        }
    }

    async fn record(
        &self,
        session_id: SessionId,
        actor: Actor,
        body: EventBody,
    ) -> anyhow::Result<SessionEvent> {
        (self.persist)(session_id, actor, body).await
    }

    async fn request(&self, request: ApprovalRequest) -> anyhow::Result<ApprovalId> {
        (self.request_approval)(request).await
    }
}

// ---------------------------------------------------------------------------
// Trace metadata (Chapter 13 groundwork)
// ---------------------------------------------------------------------------

/// Per-model-request trace metadata. Phase 1 records the model id, a request
/// hash, latency, and placeholder token/cost figures; richer accounting is
/// Chapter 13's concern.
#[derive(Debug, Clone)]
pub struct ModelRequestTrace {
    /// The model that served the request.
    pub model_id: ModelId,
    /// A hex SHA-256 over the request transcript.
    pub request_hash: String,
    /// Prompt tokens (placeholder in Phase 1).
    pub prompt_tokens: u64,
    /// Completion tokens (placeholder in Phase 1).
    pub completion_tokens: u64,
    /// Round-trip latency in milliseconds.
    pub latency_ms: u128,
    /// Estimated cost in micro-currency units (placeholder in Phase 1).
    pub cost_micros: u64,
}

// ---------------------------------------------------------------------------
// The runtime
// ---------------------------------------------------------------------------

/// The Chapter 12 runtime adapter: drives a [`ModelDriver`] through the Level-1
/// workflow with policy, approvals, artifacts, events, modes, and chronicle.
pub struct FrameworkAgentRuntime {
    models: ModelRegistry,
    policy: PolicyEngine,
    approvals: ApprovalBroker,
    subscriptions: SubscriptionHub,
    journal: RunJournal,
    sink: Box<dyn ArtifactSink>,
    /// The GitHub client the `github.*` tools call, if configured. Process-wide
    /// (one daemon token), so it lives on the runtime, not the run context.
    github: Option<Arc<dyn GitHubApi>>,
}

/// How a run terminated, before it is folded into a [`RunDisposition`].
enum Terminal {
    Completed(String),
    Cancelled,
    Failed(String),
}

impl FrameworkAgentRuntime {
    /// Assemble a runtime.
    ///
    /// `approvals` must be the same broker (a clone) the `journal`'s
    /// approval-request closure drives, so that `await_decision` observes
    /// resolutions.
    pub fn new(
        models: ModelRegistry,
        policy: PolicyEngine,
        approvals: ApprovalBroker,
        subscriptions: SubscriptionHub,
        journal: RunJournal,
        sink: Box<dyn ArtifactSink>,
    ) -> Self {
        Self {
            models,
            policy,
            approvals,
            subscriptions,
            journal,
            sink,
            github: None,
        }
    }

    /// Inject the GitHub client the `github.*` tools call. Without it those tools
    /// are unavailable (a call returns a clean failure). The daemon builds the
    /// client from the personal-mode token at startup.
    pub fn with_github(mut self, github: Arc<dyn GitHubApi>) -> Self {
        self.github = Some(github);
        self
    }

    /// The model registry (used by callers to build a [`FrameworkModelDriver`]).
    pub fn models(&self) -> &ModelRegistry {
        &self.models
    }

    /// Execute a run to a terminal disposition.
    ///
    /// Drives the Level-1 nodes around `driver`: seeds the transcript with the
    /// objective, loops model steps (streaming text, running tools through the
    /// policy/approval middleware) until `Finish`/cancel/failure, runs the
    /// review node (change-set), then the present node (chronicle +
    /// `RunCompleted`). Every state transition and event is persisted before it
    /// is published.
    pub async fn execute_run(
        &self,
        driver: &dyn ModelDriver,
        run: RunContext,
        cancel: CancellationToken,
    ) -> anyhow::Result<RunDisposition> {
        let mut run = run;
        let model_id = driver.model_id();
        let run_actor = Actor::Agent {
            agent_id: AgentId::new(),
            run_id: run.run_id,
            model: model_id.clone(),
        };

        // The run row and its `RunStarted` event were already created by the
        // `StartRun` command (STEP 1.3, `commands::apply_start_run`); this loop
        // executes an *already-started* run, so it must NOT emit a second
        // `RunStarted` (a duplicate would fold the run into `active_runs` twice
        // and show clients two starts). It resumes from the first state
        // transition: Preparing → Running (persist-then-publish, transitions
        // before exposure).
        self.transition(run.session_id, run.run_id, RunState::Preparing)
            .await?;
        self.transition(run.session_id, run.run_id, RunState::Running)
            .await?;

        // Accumulators folded into the chronicle at the terminal state.
        let mut transcript = vec![TurnItem::Objective(run.objective.clone())];
        let mut findings: Vec<String> = Vec::new();
        let mut actions: Vec<Value> = Vec::new();
        let mut changes: Vec<Value> = Vec::new();
        let mut model_requests: u64 = 0;
        let run_started = Instant::now();
        let mut wall_clock_warned = false;

        // --- Inspect/Plan/Modify/Test: the model-driven inner loop ---
        let terminal = loop {
            if cancel.is_cancelled() {
                break Terminal::Cancelled;
            }
            // Safe point: apply any queued steering between nodes.
            self.drain_steering(&mut run, &run_actor, &mut transcript)
                .await?;

            if model_requests as usize >= MAX_STEPS {
                break Terminal::Failed("model step budget exhausted".to_string());
            }

            // Wall-clock budget: MAX_STEPS bounds the number of model requests
            // but not their (or the tools') duration, so a slow provider or long
            // commands could otherwise burn unbounded time/spend. Warn once at
            // 80%, fail at the ceiling — checked at the same safe point as the
            // step budget so a run never dies mid-effect.
            let elapsed_secs = run_started.elapsed().as_secs();
            if elapsed_secs >= MAX_WALL_CLOCK_SECS {
                break Terminal::Failed("wall-clock budget exhausted".to_string());
            }
            if !wall_clock_warned && elapsed_secs >= MAX_WALL_CLOCK_SECS * 4 / 5 {
                wall_clock_warned = true;
                self.emit(
                    run.session_id,
                    run_actor.clone(),
                    EventBody::BudgetWarning {
                        run_id: run.run_id,
                        dimension: BudgetDimension::WallClock,
                        used: elapsed_secs,
                        limit: MAX_WALL_CLOCK_SECS,
                    },
                )
                .await?;
            }

            let started = Instant::now();
            let step = match driver.next_step(&transcript).await {
                Ok(step) => step,
                Err(e) => break Terminal::Failed(format!("model driver error: {e}")),
            };
            model_requests += 1;
            let trace = ModelRequestTrace {
                model_id: model_id.clone(),
                request_hash: hash_json(&transcript),
                // Token/cost fields are structurally present but UNPOPULATED: the
                // `ModelDriver` seam does not surface provider usage yet. Zero
                // here means "not measured", never "free" — real accounting needs
                // usage plumbed through the driver trait (tracked for Phase 7's
                // budget ledger).
                prompt_tokens: 0,
                completion_tokens: 0,
                latency_ms: started.elapsed().as_millis(),
                cost_micros: 0,
            };
            tracing::debug!(
                model = %trace.model_id,
                request_hash = %trace.request_hash,
                latency_ms = trace.latency_ms,
                "model request"
            );

            match step {
                ModelStep::Say(text) => {
                    self.emit(
                        run.session_id,
                        run_actor.clone(),
                        EventBody::ModelStreamDelta {
                            run_id: run.run_id,
                            text: text.clone(),
                        },
                    )
                    .await?;
                    findings.push(text.clone());
                    transcript.push(TurnItem::Assistant(text));
                }
                ModelStep::Finish { summary } => break Terminal::Completed(summary),
                ModelStep::CallTool { tool, args } => {
                    match self
                        .run_tool(&run, &run_actor, &tool, args, &mut actions, &cancel)
                        .await?
                    {
                        ToolFlow::Observation(observation) => {
                            transcript.push(TurnItem::ToolResult {
                                tool,
                                output: observation,
                            });
                            // Safe point: a completed tool call is a steering
                            // boundary.
                            self.drain_steering(&mut run, &run_actor, &mut transcript)
                                .await?;
                        }
                        // Cancellation fired while parked on an approval: stop
                        // without executing the tool.
                        ToolFlow::Cancelled => break Terminal::Cancelled,
                    }
                }
            }
        };

        // --- Review: emit a change-set if the worktree has a diff ---
        if !matches!(terminal, Terminal::Cancelled) {
            self.review_changeset(&run, &run_actor, &mut changes)
                .await?;
        }

        // --- Present: chronicle + terminal state + RunCompleted ---
        let chronicle = build_chronicle(
            &run.objective,
            &findings,
            &actions,
            &changes,
            model_requests,
        );
        let chronicle_ref = self
            .sink
            .store(
                "application/json",
                Provenance::system("run-chronicle"),
                &serde_json::to_vec_pretty(&chronicle)?,
            )
            .await?;

        let (state, disposition) = match terminal {
            Terminal::Completed(summary) => (
                RunState::Completed,
                RunDisposition::Completed {
                    summary: Some(summary),
                },
            ),
            Terminal::Cancelled => (
                RunState::Cancelled,
                RunDisposition::Cancelled {
                    reason: Some("run cancelled".to_string()),
                },
            ),
            Terminal::Failed(reason) => (RunState::Failed, RunDisposition::Failed { reason }),
        };

        self.transition(run.session_id, run.run_id, state).await?;
        self.emit(
            run.session_id,
            run_actor,
            EventBody::RunCompleted {
                run_id: run.run_id,
                disposition: disposition.clone(),
                chronicle: chronicle_ref,
            },
        )
        .await?;

        Ok(disposition)
    }

    // -- event helpers -----------------------------------------------------

    /// Persist an event through the journal, then publish it (persist before
    /// publish, RULE: no client observes an uncommitted event).
    async fn emit(
        &self,
        session_id: SessionId,
        actor: Actor,
        body: EventBody,
    ) -> anyhow::Result<SessionEvent> {
        let event = self.journal.record(session_id, actor, body).await?;
        self.subscriptions.publish(session_id, event.clone());
        Ok(event)
    }

    /// Persist a run-state transition (the journal updates the `runs` row in the
    /// same step) and publish it.
    async fn transition(
        &self,
        session_id: SessionId,
        run_id: RunId,
        state: RunState,
    ) -> anyhow::Result<()> {
        self.emit(
            session_id,
            Actor::System,
            EventBody::RunStateChanged { run_id, state },
        )
        .await?;
        Ok(())
    }

    /// Drain queued steering, injecting each into the transcript and emitting
    /// `SteeringApplied`. Called only at safe points (between nodes / after a
    /// completed tool call).
    async fn drain_steering(
        &self,
        run: &mut RunContext,
        run_actor: &Actor,
        transcript: &mut Vec<TurnItem>,
    ) -> anyhow::Result<()> {
        let session_id = run.session_id;
        let run_id = run.run_id;
        let mut applied = Vec::new();
        if let Some(rx) = run.steering.as_mut() {
            while let Ok(text) = rx.try_recv() {
                applied.push(text);
            }
        }
        for text in applied {
            transcript.push(TurnItem::Steering(text));
            self.emit(
                session_id,
                run_actor.clone(),
                EventBody::SteeringApplied { run_id },
            )
            .await?;
        }
        Ok(())
    }

    // -- tool middleware ---------------------------------------------------

    /// Run one model-proposed tool through the middleware: map to a
    /// [`ProposedAction`], evaluate policy, request+await approval when required,
    /// execute under the granted scope, and emit `ToolStarted`/`ToolCompleted`.
    /// Returns the compacted observation to feed back to the model, or
    /// [`ToolFlow::Cancelled`] if the run was cancelled while parked on approval.
    async fn run_tool(
        &self,
        run: &RunContext,
        run_actor: &Actor,
        tool: &str,
        args: Value,
        actions: &mut Vec<Value>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<ToolFlow> {
        // (a) map the call to a typed tool + proposed action.
        let prepared = match self.prepare(tool, &args, run).await {
            Ok(prepared) => prepared,
            Err(message) => {
                self.emit(
                    run.session_id,
                    run_actor.clone(),
                    EventBody::ToolCompleted {
                        run_id: run.run_id,
                        tool: tool.to_string(),
                        outcome: ToolOutcome::Failed {
                            message: message.clone(),
                        },
                        artifact: None,
                    },
                )
                .await?;
                actions.push(action_digest(tool, "failed", None));
                return Ok(ToolFlow::Observation(format!("tool error: {message}")));
            }
        };

        // (b) evaluate policy under the mode overlay.
        let decision = self.policy.evaluate(&prepared.action, &self.eval_ctx(run));
        match decision.decision {
            Decision::Deny => {
                let reason = decision
                    .reasons
                    .first()
                    .map(|r| r.message.clone())
                    .unwrap_or_else(|| "denied by policy".to_string());
                // (c) on Deny: emit a denial completion and DO NOT execute.
                self.emit(
                    run.session_id,
                    run_actor.clone(),
                    EventBody::ToolCompleted {
                        run_id: run.run_id,
                        tool: tool.to_string(),
                        outcome: ToolOutcome::Failed {
                            message: format!("policy denied: {reason}"),
                        },
                        artifact: None,
                    },
                )
                .await?;
                actions.push(action_digest(tool, "denied", None));
                return Ok(ToolFlow::Observation(format!("policy denied: {reason}")));
            }
            Decision::RequireApproval => {
                // (c) park the run in WaitingForApproval until an approver
                // resolves. Publish ToolProposed last, so no ledger append
                // races the approver's resolution while the run is parked.
                let capabilities = decision
                    .capability_grant
                    .clone()
                    .map(|grant| vec![grant.capability])
                    .unwrap_or_default();
                let risk = Risk {
                    level: RiskLevel::Medium,
                    reasons: decision.reasons.iter().map(|r| r.message.clone()).collect(),
                };
                let approval_id = self
                    .journal
                    .request(ApprovalRequest {
                        session_id: run.session_id,
                        run_id: run.run_id,
                        action: prepared.action.clone(),
                        risk,
                        capabilities,
                    })
                    .await?;
                self.transition(run.session_id, run.run_id, RunState::WaitingForApproval)
                    .await?;
                self.emit(
                    run.session_id,
                    run_actor.clone(),
                    EventBody::ToolProposed {
                        run_id: run.run_id,
                        approval_id,
                        action: prepared.action.clone(),
                    },
                )
                .await?;

                // Park on the decision, but never block a cancelled run: race the
                // approval against cancellation. If cancellation wins, stop here
                // (do not run the tool) and let the loop drive the run to
                // Cancelled.
                let decision = tokio::select! {
                    decision = self.approvals.await_decision(approval_id) => decision?,
                    _ = cancel.cancelled() => return Ok(ToolFlow::Cancelled),
                };
                self.transition(run.session_id, run.run_id, RunState::Running)
                    .await?;
                if decision != ApprovalDecision::Approve {
                    self.emit(
                        run.session_id,
                        run_actor.clone(),
                        EventBody::ToolCompleted {
                            run_id: run.run_id,
                            tool: tool.to_string(),
                            outcome: ToolOutcome::Failed {
                                message: "approval rejected".to_string(),
                            },
                            artifact: None,
                        },
                    )
                    .await?;
                    actions.push(action_digest(tool, "rejected", None));
                    return Ok(ToolFlow::Observation("approval rejected".to_string()));
                }
            }
            Decision::Allow => {}
        }

        // (d) execute under the granted scope.
        self.emit(
            run.session_id,
            run_actor.clone(),
            EventBody::ToolStarted {
                run_id: run.run_id,
                tool: tool.to_string(),
                args_digest: hash_json(&args),
            },
        )
        .await?;
        let (observation, artifact, outcome) = self.execute_prepared(prepared, run).await;
        // (e/f) emit completion referencing any spilled artifact.
        self.emit(
            run.session_id,
            run_actor.clone(),
            EventBody::ToolCompleted {
                run_id: run.run_id,
                tool: tool.to_string(),
                outcome: outcome.clone(),
                artifact: artifact.clone(),
            },
        )
        .await?;
        actions.push(action_digest(
            tool,
            outcome_label(&outcome),
            artifact.as_ref().map(|a| a.id),
        ));
        Ok(ToolFlow::Observation(observation))
    }

    /// Map a tool call to its typed input and [`ProposedAction`]. Applying a
    /// patch is modelled as a `WritePatch` (semantically a write), so the patch
    /// is spilled to an artifact first and referenced by id.
    async fn prepare(
        &self,
        tool: &str,
        args: &Value,
        run: &RunContext,
    ) -> Result<Prepared, String> {
        match tool {
            Shell::NAME => {
                let request = parse_command_request(args, &run.worktree)?;
                let action = Shell::proposed_action(&request);
                Ok(Prepared {
                    action,
                    tool: PreparedTool::Shell(request),
                })
            }
            ReadFile::NAME => {
                let input = parse_read_file(args)?;
                let action = ReadFile::proposed_action(&input);
                Ok(Prepared {
                    action,
                    tool: PreparedTool::ReadFile(input),
                })
            }
            Search::NAME => {
                let input = parse_search(args)?;
                let action = Search::proposed_action(&self.read_scope(run));
                Ok(Prepared {
                    action,
                    tool: PreparedTool::Search(input),
                })
            }
            GitDiff::NAME => {
                let input = GitDiffInput {
                    cwd: run.worktree.clone(),
                };
                let action = GitDiff::proposed_action(&input);
                Ok(Prepared {
                    action,
                    tool: PreparedTool::GitDiff(input),
                })
            }
            ApplyPatch::NAME => {
                let input = parse_apply_patch(args, &run.worktree)?;
                let stored = self
                    .sink
                    .store(
                        "text/x-diff",
                        Provenance::tool_output(ApplyPatch::NAME, run.run_id),
                        input.patch.as_bytes(),
                    )
                    .await
                    .map_err(|e| format!("could not stage patch artifact: {e}"))?;
                Ok(Prepared {
                    action: ProposedAction::WritePatch { patch: stored.id },
                    tool: PreparedTool::ApplyPatch(input),
                })
            }
            GetPullRequest::NAME => {
                let repo = self.github_target(run)?;
                let input = parse_get_pull_request(args)?;
                Ok(Prepared {
                    action: GetPullRequest::proposed_action(),
                    tool: PreparedTool::GitHubGetPr { repo, input },
                })
            }
            ListCheckRuns::NAME => {
                let repo = self.github_target(run)?;
                let input = parse_list_check_runs(args)?;
                Ok(Prepared {
                    action: ListCheckRuns::proposed_action(),
                    tool: PreparedTool::GitHubListChecks { repo, input },
                })
            }
            CreateDraftPullRequest::NAME => {
                let repo = self.github_target(run)?;
                let input = parse_create_draft_pull_request(args)?;
                Ok(Prepared {
                    action: CreateDraftPullRequest::proposed_action(&repo),
                    tool: PreparedTool::GitHubCreateDraftPr { repo, input },
                })
            }
            UpdatePullRequestTool::NAME => {
                let repo = self.github_target(run)?;
                let input = parse_update_pull_request(args)?;
                Ok(Prepared {
                    action: UpdatePullRequestTool::proposed_action(&repo),
                    tool: PreparedTool::GitHubUpdatePr { repo, input },
                })
            }
            CreateCheckRunSummary::NAME => {
                let repo = self.github_target(run)?;
                let input = parse_create_check_run(args)?;
                Ok(Prepared {
                    action: CreateCheckRunSummary::proposed_action(&repo),
                    tool: PreparedTool::GitHubCheckSummary { repo, input },
                })
            }
            other => Err(format!("unknown tool `{other}`")),
        }
    }

    /// The [`SourceProvenance`] of a just-read file (Phase 3 STEP 3.4). If the
    /// IDE reported an unsaved buffer for this path, compare its digest to the
    /// on-disk bytes: a match means the editor is in sync (`filesystem`); a
    /// mismatch (or an unreadable file) means the disk content is stale relative
    /// to the editor (`unsaved-ide-buffer`). With no dirty buffer, it is a plain
    /// filesystem read.
    async fn read_provenance(&self, path: &Path, run: &RunContext) -> SourceProvenance {
        let path_str = path.to_string_lossy();
        let dirty = run.ide_dirty_buffers.iter().find(|buffer| {
            path_str == buffer.path.as_str()
                || path_str.ends_with(&buffer.path)
                || buffer.path.ends_with(path_str.as_ref())
        });
        match dirty {
            Some(buffer) => match tokio::fs::read(path).await {
                Ok(bytes) if digest_bytes(&bytes) == buffer.sha256 => SourceProvenance::Filesystem,
                _ => SourceProvenance::UnsavedIdeBuffer,
            },
            None => SourceProvenance::Filesystem,
        }
    }

    /// Resolve the GitHub target for a `github.*` tool call: the client must be
    /// injected and the run must name a repository. A clear error otherwise lets
    /// the model see why the tool is unavailable.
    fn github_target(&self, run: &RunContext) -> Result<RepoId, String> {
        if self.github.is_none() {
            return Err("github is not configured (no token available)".to_string());
        }
        run.github_repo
            .clone()
            .ok_or_else(|| "no github repository is configured for this run".to_string())
    }

    /// Execute a prepared tool under the scopes minted from the policy for this
    /// run's mode/context, returning `(observation, artifact, outcome)`.
    async fn execute_prepared(
        &self,
        prepared: Prepared,
        run: &RunContext,
    ) -> (String, Option<ArtifactRef>, ToolOutcome) {
        let read_scope = self.read_scope(run);
        let write_scope = self.write_scope(run);
        let command_scope = self.policy.command_scope();
        match prepared.tool {
            PreparedTool::Shell(request) => {
                match Shell::execute(
                    &request,
                    &write_scope,
                    &command_scope,
                    &*self.sink,
                    run.run_id,
                )
                .await
                {
                    Ok(outcome) => {
                        let observation = outcome.salient.render();
                        let artifact = outcome.stdout_ref.clone();
                        let result = if outcome.success() {
                            ToolOutcome::Succeeded
                        } else {
                            ToolOutcome::Failed {
                                message: describe_exit(&outcome),
                            }
                        };
                        (observation, artifact, result)
                    }
                    Err(e) => (
                        format!("shell.run error: {e}"),
                        None,
                        ToolOutcome::Failed {
                            message: e.code().to_string(),
                        },
                    ),
                }
            }
            PreparedTool::ReadFile(input) => match ReadFile::execute(&input, &read_scope).await {
                Ok(excerpt) => {
                    // Label the excerpt with its origin (Phase 3 STEP 3.4). The
                    // common `filesystem` case is left unmarked to keep the trace
                    // quiet; a read whose disk bytes diverge from an unsaved editor
                    // buffer is flagged so the model and the trace know the content
                    // may be stale relative to the editor.
                    let observation = match self.read_provenance(&excerpt.path, run).await {
                        SourceProvenance::Filesystem => excerpt.content,
                        other => format!("[source: {}]\n{}", other.label(), excerpt.content),
                    };
                    (observation, None, ToolOutcome::Succeeded)
                }
                Err(e) => (
                    format!("workspace.read_file error: {e}"),
                    None,
                    ToolOutcome::Failed {
                        message: e.code().to_string(),
                    },
                ),
            },
            PreparedTool::Search(input) => match Search::execute(&input, &read_scope).await {
                Ok(results) => (render_search(&results), None, ToolOutcome::Succeeded),
                Err(e) => (
                    format!("workspace.search error: {e}"),
                    None,
                    ToolOutcome::Failed {
                        message: e.code().to_string(),
                    },
                ),
            },
            PreparedTool::GitDiff(input) => {
                match GitDiff::execute(
                    &input,
                    &write_scope,
                    &command_scope,
                    &*self.sink,
                    run.run_id,
                )
                .await
                {
                    Ok(diff) => {
                        let observation = if diff.is_empty {
                            "worktree is clean".to_string()
                        } else {
                            diff.diff.clone()
                        };
                        (observation, diff.artifact.clone(), ToolOutcome::Succeeded)
                    }
                    Err(e) => (
                        format!("git.diff error: {e}"),
                        None,
                        ToolOutcome::Failed {
                            message: e.code().to_string(),
                        },
                    ),
                }
            }
            PreparedTool::ApplyPatch(input) => {
                match ApplyPatch::execute(&input, &write_scope, &command_scope).await {
                    Ok(_) => ("patch applied".to_string(), None, ToolOutcome::Succeeded),
                    Err(e) => (
                        format!("git.apply_patch error: {e}"),
                        None,
                        ToolOutcome::Failed {
                            message: e.code().to_string(),
                        },
                    ),
                }
            }
            PreparedTool::GitHubGetPr { repo, input } => match self.github.as_ref() {
                None => github_unconfigured(),
                Some(client) => match client.get_pull_request(&repo, input.number).await {
                    Ok(pr) => (render_pull_request(&pr), None, ToolOutcome::Succeeded),
                    Err(e) => github_failure("github.get_pull_request", &e),
                },
            },
            PreparedTool::GitHubListChecks { repo, input } => match self.github.as_ref() {
                None => github_unconfigured(),
                Some(client) => match client.list_check_runs(&repo, &input.git_ref).await {
                    Ok(runs) => (render_check_runs(&runs), None, ToolOutcome::Succeeded),
                    Err(e) => github_failure("github.list_check_runs", &e),
                },
            },
            PreparedTool::GitHubCreateDraftPr { repo, input } => match self.github.as_ref() {
                None => github_unconfigured(),
                Some(client) => {
                    let request = new_pull_request(&input);
                    match client
                        .create_draft_pull_request(&repo, &request, &input.idempotency_key)
                        .await
                    {
                        Ok(pr) => (
                            format!("opened draft PR #{} — {}", pr.number, pr.html_url),
                            None,
                            ToolOutcome::Succeeded,
                        ),
                        Err(e) => github_failure("github.create_draft_pull_request", &e),
                    }
                }
            },
            PreparedTool::GitHubUpdatePr { repo, input } => match self.github.as_ref() {
                None => github_unconfigured(),
                Some(client) => match client
                    .update_pull_request(&repo, input.number, &input.request)
                    .await
                {
                    Ok(pr) => (
                        format!("updated PR #{} [{}]", pr.number, pr.state),
                        None,
                        ToolOutcome::Succeeded,
                    ),
                    Err(e) => github_failure("github.update_pull_request", &e),
                },
            },
            PreparedTool::GitHubCheckSummary { repo, input } => match self.github.as_ref() {
                None => github_unconfigured(),
                Some(client) => {
                    match client.create_check_run_summary(&repo, &input.request).await {
                        Ok(check) => (
                            format!(
                                "posted check-run summary `{}` [{}]",
                                check.name, check.status
                            ),
                            None,
                            ToolOutcome::Succeeded,
                        ),
                        Err(e) => github_failure("github.create_check_run_summary", &e),
                    }
                }
            },
        }
    }

    /// The review node: if the worktree has a diff, spill it as a change-set
    /// artifact and emit `PatchProposed`. Loop-issued (not model-proposed), so
    /// it runs without approval — it is a trusted daemon diff of the run's own
    /// worktree. A non-repository worktree simply yields no change-set.
    async fn review_changeset(
        &self,
        run: &RunContext,
        run_actor: &Actor,
        changes: &mut Vec<Value>,
    ) -> anyhow::Result<()> {
        let write_scope = self.write_scope(run);
        let command_scope = self.policy.command_scope();
        let diff = GitDiff::execute(
            &GitDiffInput {
                cwd: run.worktree.clone(),
            },
            &write_scope,
            &command_scope,
            &*self.sink,
            run.run_id,
        )
        .await;
        if let Ok(diff) = diff {
            if !diff.is_empty {
                if let Some(artifact) = diff.artifact.clone() {
                    let changeset_id = ChangeSetId::new();
                    self.emit(
                        run.session_id,
                        run_actor.clone(),
                        EventBody::PatchProposed {
                            run_id: run.run_id,
                            changeset_id,
                            artifact: artifact.clone(),
                        },
                    )
                    .await?;
                    changes.push(json!({
                        "changeset_id": changeset_id.to_string(),
                        "artifact": artifact.id.to_string(),
                        "byte_length": artifact.byte_length,
                    }));
                }
            }
        }
        Ok(())
    }

    // -- scope helpers -----------------------------------------------------

    fn eval_ctx(&self, run: &RunContext) -> EvalContext {
        EvalContext {
            repository: run.repository.clone(),
            worktree: run.worktree.clone(),
            mode: mode_overlay(run.mode),
        }
    }

    fn read_scope(&self, run: &RunContext) -> PathScope {
        self.policy.file_read_scope(&self.eval_ctx(run))
    }

    fn write_scope(&self, run: &RunContext) -> PathScope {
        self.policy.file_write_scope(&self.eval_ctx(run))
    }
}

/// The outcome of driving one tool call through the middleware.
enum ToolFlow {
    /// The compacted observation to feed back to the model.
    Observation(String),
    /// The run was cancelled while parked on an approval; the loop must stop
    /// without executing the tool.
    Cancelled,
}

/// A tool call resolved to its typed input plus the action policy evaluates.
struct Prepared {
    action: ProposedAction,
    tool: PreparedTool,
}

/// A model tool call parsed into its typed, executable input.
enum PreparedTool {
    Shell(CommandRequest),
    ReadFile(ReadFileInput),
    Search(SearchInput),
    GitDiff(GitDiffInput),
    ApplyPatch(ApplyPatchInput),
    GitHubGetPr {
        repo: RepoId,
        input: GetPullRequestInput,
    },
    GitHubListChecks {
        repo: RepoId,
        input: ListCheckRunsInput,
    },
    GitHubCreateDraftPr {
        repo: RepoId,
        input: CreateDraftPullRequestInput,
    },
    GitHubUpdatePr {
        repo: RepoId,
        input: UpdatePullRequestInput,
    },
    GitHubCheckSummary {
        repo: RepoId,
        input: CreateCheckRunInput,
    },
}

// ---------------------------------------------------------------------------
// Argument parsing and observation rendering
// ---------------------------------------------------------------------------

/// The tool-result tuple for a `github.*` call made without a configured client.
fn github_unconfigured() -> (String, Option<ArtifactRef>, ToolOutcome) {
    (
        "github is not configured (no token available)".to_string(),
        None,
        ToolOutcome::Failed {
            message: "github.unconfigured".to_string(),
        },
    )
}

/// The tool-result tuple for a failed `github.*` API call. The error's `Display`
/// never contains the token (the client keeps it out of every error).
fn github_failure(tool: &str, error: &GitHubError) -> (String, Option<ArtifactRef>, ToolOutcome) {
    (
        format!("{tool} error: {error}"),
        None,
        ToolOutcome::Failed {
            message: "github.api-error".to_string(),
        },
    )
}

fn parse_command_request(args: &Value, worktree: &Path) -> Result<CommandRequest, String> {
    let program = args
        .get("program")
        .and_then(Value::as_str)
        .ok_or("shell.run requires a string `program`")?;
    let cmd_args = args
        .get("args")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let cwd = args
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| worktree.to_path_buf());
    let environment = args
        .get("environment")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|value| EnvironmentBinding::new(k, value)))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let timeout = std::time::Duration::from_secs(
        args.get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_SHELL_TIMEOUT_SECS),
    );
    Ok(CommandRequest {
        program: PathBuf::from(program),
        args: cmd_args,
        cwd,
        environment,
        timeout,
    })
}

fn parse_read_file(args: &Value) -> Result<ReadFileInput, String> {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or("workspace.read_file requires a string `path`")?;
    let range = args.get("range").and_then(Value::as_array).and_then(|r| {
        match (
            r.first().and_then(Value::as_u64),
            r.get(1).and_then(Value::as_u64),
        ) {
            (Some(start), Some(end)) => Some((start as usize, end as usize)),
            _ => None,
        }
    });
    Ok(ReadFileInput {
        path: PathBuf::from(path),
        range,
    })
}

fn parse_search(args: &Value) -> Result<SearchInput, String> {
    let pattern = args
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or("workspace.search requires a string `pattern`")?;
    let glob = args.get("glob").and_then(Value::as_str).map(str::to_string);
    Ok(SearchInput {
        pattern: pattern.to_string(),
        glob,
    })
}

fn parse_apply_patch(args: &Value, worktree: &Path) -> Result<ApplyPatchInput, String> {
    let patch = args
        .get("patch")
        .and_then(Value::as_str)
        .ok_or("git.apply_patch requires a string `patch`")?;
    let cwd = args
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| worktree.to_path_buf());
    Ok(ApplyPatchInput {
        cwd,
        patch: patch.to_string(),
    })
}

fn render_search(results: &crate::tools::SearchResults) -> String {
    let mut out = String::new();
    for m in &results.matches {
        out.push_str(&format!(
            "{}:{}: {}\n",
            m.path.display(),
            m.line_number,
            m.line
        ));
    }
    if results.truncated {
        out.push_str("… results truncated …\n");
    }
    if out.is_empty() {
        out.push_str("no matches\n");
    }
    out
}

fn describe_exit(outcome: &crate::tools::ShellOutcome) -> String {
    if outcome.timed_out {
        "command timed out".to_string()
    } else {
        match outcome.exit_code {
            Some(code) => format!("exited with status {code}"),
            None => "process killed".to_string(),
        }
    }
}

fn outcome_label(outcome: &ToolOutcome) -> &'static str {
    match outcome {
        ToolOutcome::Succeeded => "succeeded",
        ToolOutcome::Failed { .. } => "failed",
        _ => "unknown",
    }
}

fn action_digest(tool: &str, outcome: &str, artifact: Option<ArtifactId>) -> Value {
    json!({
        "tool": tool,
        "outcome": outcome,
        "artifact": artifact.map(|id| id.to_string()),
    })
}

/// A hex SHA-256 over the JSON serialization of `value` — the request/args
/// digest used for trace metadata and `ToolStarted.args_digest`.
fn hash_json<T: Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    hex::encode(Sha256::digest(&bytes))
}

/// Fold the run's observations into a [Chapter 20 `SessionChronicle`]-shaped
/// JSON value: objective, findings, actions, changes, verification, costs, and
/// unresolved questions.
fn build_chronicle(
    objective: &str,
    findings: &[String],
    actions: &[Value],
    changes: &[Value],
    model_requests: u64,
) -> Value {
    json!({
        "objective": objective,
        "specification": Value::Null,
        "plan_versions": [],
        "investigations": findings,
        "decisions": [],
        "actions": actions,
        "changes": changes,
        "verification": [],
        "costs": {
            "model_requests": model_requests,
            "tokens": 0,
            "cost_micros": 0,
        },
        "unresolved": [],
    })
}

// ---------------------------------------------------------------------------
// FrameworkModelDriver — the live provider path (feature-gated)
// ---------------------------------------------------------------------------

/// A [`ModelDriver`] backed by a framework `ChatClient`
/// (`agent_framework_openai::OpenAIChatCompletionClient`).
///
/// It translates the loop's [`TurnItem`] transcript into framework
/// [`Message`](agent_framework_core::types::Message)s, advertises the Phase 1
/// tools as declaration-only function tools, calls
/// [`ChatClient::get_response`](agent_framework_core::client::ChatClient::get_response),
/// and maps the returned turn back to a [`ModelStep`]: a function call becomes
/// [`ModelStep::CallTool`], any other completed turn becomes
/// [`ModelStep::Finish`] carrying its text.
///
/// This is a focused implementation compiled behind `provider-openai`; a live
/// endpoint is not available in this environment, so it has no live test. The
/// transcript translation is intentionally simple (tool results are replayed as
/// user turns rather than threaded by `call_id`), which is sufficient for the
/// Phase 1 single-tool-at-a-time loop.
#[cfg(feature = "provider-openai")]
pub struct FrameworkModelDriver {
    client: agent_framework_openai::OpenAIChatCompletionClient,
    model_id: ModelId,
}

#[cfg(feature = "provider-openai")]
impl FrameworkModelDriver {
    /// Wrap a constructed client and record the model id it serves.
    pub fn new(
        client: agent_framework_openai::OpenAIChatCompletionClient,
        model_id: ModelId,
    ) -> Self {
        Self { client, model_id }
    }

    /// Build a driver from the registry by resolving `model_id` to a client.
    pub fn from_registry(models: &ModelRegistry, model_id: ModelId) -> anyhow::Result<Self> {
        let client = models
            .client_for(&model_id)
            .map_err(|e| anyhow::anyhow!("could not build client for {model_id}: {e}"))?;
        Ok(Self::new(client, model_id))
    }

    /// The Phase 1 tools advertised to the model as declaration-only function
    /// tools (the loop executes them; the framework never does).
    fn tool_definitions() -> Vec<agent_framework_core::tools::ToolDefinition> {
        use agent_framework_core::tools::{ApprovalMode, ToolDefinition, ToolKind};
        let decl = |name: &str, description: &str, parameters: Value| ToolDefinition {
            name: name.to_string(),
            description: description.to_string(),
            parameters,
            kind: ToolKind::Function,
            approval_mode: ApprovalMode::NeverRequire,
            executor: None,
        };
        vec![
            decl(
                Shell::NAME,
                "Run an allow-listed program in the worktree.",
                json!({
                    "type": "object",
                    "properties": {
                        "program": {"type": "string"},
                        "args": {"type": "array", "items": {"type": "string"}}
                    },
                    "required": ["program"]
                }),
            ),
            decl(
                ReadFile::NAME,
                "Read a line-numbered excerpt of a file.",
                json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
            ),
            decl(
                Search::NAME,
                "Search the repository for a pattern.",
                json!({
                    "type": "object",
                    "properties": {"pattern": {"type": "string"}, "glob": {"type": "string"}},
                    "required": ["pattern"]
                }),
            ),
            decl(
                GitDiff::NAME,
                "Show the worktree diff.",
                json!({"type": "object", "properties": {}}),
            ),
            decl(
                ApplyPatch::NAME,
                "Apply a unified-diff patch to the worktree.",
                json!({
                    "type": "object",
                    "properties": {"patch": {"type": "string"}},
                    "required": ["patch"]
                }),
            ),
            decl(
                GetPullRequest::NAME,
                "Fetch a GitHub pull request by number (read-only).",
                json!({
                    "type": "object",
                    "properties": {"number": {"type": "integer"}},
                    "required": ["number"]
                }),
            ),
            decl(
                ListCheckRuns::NAME,
                "List the GitHub check runs for a git ref (read-only).",
                json!({
                    "type": "object",
                    "properties": {"ref": {"type": "string"}},
                    "required": ["ref"]
                }),
            ),
            decl(
                CreateDraftPullRequest::NAME,
                "Open a draft GitHub pull request (requires approval).",
                json!({
                    "type": "object",
                    "properties": {
                        "title": {"type": "string"},
                        "head": {"type": "string"},
                        "base": {"type": "string"},
                        "body": {"type": "string"}
                    },
                    "required": ["title", "head", "base"]
                }),
            ),
            decl(
                UpdatePullRequestTool::NAME,
                "Update a GitHub pull request's title/body/state (requires approval).",
                json!({
                    "type": "object",
                    "properties": {
                        "number": {"type": "integer"},
                        "title": {"type": "string"},
                        "body": {"type": "string"},
                        "state": {"type": "string"}
                    },
                    "required": ["number"]
                }),
            ),
            decl(
                CreateCheckRunSummary::NAME,
                "Post a GitHub check-run summary against a commit (requires approval).",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "head_sha": {"type": "string"},
                        "summary": {"type": "string"},
                        "conclusion": {"type": "string"}
                    },
                    "required": ["name", "head_sha", "summary"]
                }),
            ),
        ]
    }

    fn to_messages(transcript: &[TurnItem]) -> Vec<agent_framework_core::types::Message> {
        use agent_framework_core::types::{Message, Role};
        let mut messages = vec![Message::system(
            "You are a coding agent. Use the provided tools to inspect and modify \
             the repository, then finish with a short summary.",
        )];
        for item in transcript {
            let message = match item {
                TurnItem::Objective(text) => Message::user(text.clone()),
                TurnItem::Assistant(text) => Message::assistant(text.clone()),
                TurnItem::ToolResult { tool, output } => {
                    Message::new(Role::tool(), format!("[{tool}] {output}"))
                }
                TurnItem::Steering(text) => Message::user(text.clone()),
            };
            messages.push(message);
        }
        messages
    }
}

#[cfg(feature = "provider-openai")]
#[async_trait]
impl ModelDriver for FrameworkModelDriver {
    fn model_id(&self) -> ModelId {
        self.model_id.clone()
    }

    async fn next_step(&self, transcript: &[TurnItem]) -> anyhow::Result<ModelStep> {
        use agent_framework_core::client::ChatClient;
        use agent_framework_core::types::ChatOptions;

        let mut options = ChatOptions::new();
        options.tools = Self::tool_definitions();

        let response = self
            .client
            .get_response(Self::to_messages(transcript), options)
            .await
            .map_err(|e| anyhow::anyhow!("model request failed: {e}"))?;

        // A function call in the returned turn becomes a tool call.
        if let Some(message) = response.messages.last() {
            if let Some(call) = message.function_calls().into_iter().next() {
                let args = call
                    .parse_arguments()
                    .map(|map| serde_json::to_value(map).unwrap_or(Value::Null))
                    .unwrap_or(Value::Null);
                return Ok(ModelStep::CallTool {
                    tool: call.name.clone(),
                    args,
                });
            }
        }

        // Otherwise the completed turn is the final answer.
        let text = response.text();
        Ok(ModelStep::Finish {
            summary: if text.is_empty() {
                "run complete".to_string()
            } else {
                text
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Unit tests (the loop's integration tests live in tests/agent_it.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_overlay_enforces_read_only_modes() {
        assert!(!mode_overlay(AgentMode::Explore).write_allowed);
        assert!(!mode_overlay(AgentMode::Explore).command_allowed);
        assert!(!mode_overlay(AgentMode::Ask).write_allowed);
        assert!(!mode_overlay(AgentMode::Plan).write_allowed);
        assert!(mode_overlay(AgentMode::Plan).command_allowed);
        assert!(mode_overlay(AgentMode::Build).write_allowed);
        assert!(mode_overlay(AgentMode::Build).command_allowed);
        assert!(!mode_overlay(AgentMode::Review).write_allowed);
        // An unknown mode is the most restrictive.
        assert!(!mode_overlay(AgentMode::Unknown).write_allowed);
    }

    #[tokio::test]
    async fn scripted_driver_yields_then_finishes() {
        let driver = ScriptedDriver::new(vec![
            ModelStep::Say("hi".to_string()),
            ModelStep::Finish {
                summary: "done".to_string(),
            },
        ]);
        assert_eq!(
            driver.next_step(&[]).await.unwrap(),
            ModelStep::Say("hi".to_string())
        );
        assert!(matches!(
            driver.next_step(&[]).await.unwrap(),
            ModelStep::Finish { .. }
        ));
        // Draining past the end keeps yielding Finish, never hangs.
        assert!(matches!(
            driver.next_step(&[]).await.unwrap(),
            ModelStep::Finish { .. }
        ));
    }

    #[test]
    fn cancellation_token_flips_on_cancel() {
        let (handle, token) = cancellation();
        assert!(!token.is_cancelled());
        handle.cancel();
        assert!(token.is_cancelled());
        // A `never` token stays false even with its source dropped.
        assert!(!CancellationToken::never().is_cancelled());
    }

    #[test]
    fn chronicle_has_the_chapter20_shape() {
        let chronicle = build_chronicle(
            "diagnose",
            &["found it".to_string()],
            &[action_digest("shell.run", "succeeded", None)],
            &[],
            3,
        );
        assert_eq!(chronicle["objective"], "diagnose");
        assert_eq!(chronicle["investigations"][0], "found it");
        assert_eq!(chronicle["actions"][0]["tool"], "shell.run");
        assert_eq!(chronicle["costs"]["model_requests"], 3);
        assert!(chronicle.get("unresolved").is_some());
    }
}
