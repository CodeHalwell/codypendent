//! The concrete [`RunExecutor`]: wraps the runtime agent loop.
//!
//! This lives in the assembly binary because it needs BOTH the daemon (the pool,
//! ledger, artifact store, subscription hub, approval broker, and the
//! [`recovery::fail_run`] helper) and the runtime ([`FrameworkAgentRuntime`],
//! [`FrameworkModelDriver`], the model registry/policy). The daemon crate cannot
//! name the runtime, so this seam is the one place both worlds meet.
//!
//! It also owns the shared [`SubscriptionHub`] + [`ApprovalBroker`] the server
//! binds to (via [`RunExecutor::collaborators`]): a run's events are published to
//! this hub — the same one the server forwards to attached clients — and
//! approvals are driven on this broker — the same one the server's command
//! processor resolves against. Without that sharing a headless client would
//! never observe the run it started.
//!
//! ## The SQLite boundary
//!
//! The runtime reaches the ledger + artifact store through a pool-erased
//! [`RunJournal`] and [`ArtifactSink`] (it cannot name `SqlitePool`; see the
//! agent-module docs). This crate *can* name the pool, so [`RuntimeExecutor`]
//! builds those from plain closures rather than the macros the runtime's own
//! integration tests use.

use chrono::Utc;
use codypendent_daemon::approvals::ApprovalBroker;
use codypendent_daemon::artifacts::{ArtifactStore, Provenance};
use codypendent_daemon::executor::{RunExecutor, RunLaunch};
use codypendent_daemon::policy::PolicyEngine;
use codypendent_daemon::subscriptions::SubscriptionHub;
use codypendent_daemon::{ledger, projections, recovery};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{Actor, DataClassification, EventBody, SessionEvent, SessionId};
use codypendent_runtime::agent::{
    ApprovalRequest, CancellationToken, FrameworkAgentRuntime, FrameworkModelDriver, RunContext,
    RunJournal,
};
use codypendent_runtime::models::{load_models, resolve_model, ModelPolicy, ModelRegistry};
use codypendent_runtime::tools::{ArtifactSink, ClosureSink};
use sqlx::SqlitePool;
use tracing::{error, info, warn};

/// Executes accepted runs by driving the runtime agent loop. Cheap to clone —
/// every field is an `Arc`-backed handle or a plain (clonable) path bundle.
#[derive(Clone)]
pub struct RuntimeExecutor {
    pool: SqlitePool,
    paths: RuntimePaths,
    subscriptions: SubscriptionHub,
    approvals: ApprovalBroker,
}

impl RuntimeExecutor {
    /// Build an executor over the daemon's pool + paths, minting the shared
    /// fan-out + approval broker the server binds to via [`Self::collaborators`].
    pub fn new(pool: SqlitePool, paths: RuntimePaths) -> Self {
        let subscriptions = SubscriptionHub::new();
        // Bind the broker to the SAME hub the server fans out to, so an
        // `ApprovalRequested` raised by the agent loop reaches attached clients
        // live (not only on re-attach catch-up).
        let approvals = ApprovalBroker::new().with_subscriptions(subscriptions.clone());
        Self {
            pool,
            paths,
            subscriptions,
            approvals,
        }
    }

    /// The content-addressed store rooted at `<data_dir>/artifacts`.
    fn artifacts(&self) -> ArtifactStore {
        ArtifactStore::new(self.paths.data_dir.join("artifacts"))
    }

    /// Load a model registry + a Phase-1 policy from `<data_dir>/models.toml`,
    /// or an error string when none is configured. In a bare environment (no
    /// endpoint, no config) this is the expected path — the run is then failed
    /// cleanly by the caller.
    fn load_registry(&self) -> Result<(ModelRegistry, ModelPolicy), String> {
        let path = self.paths.data_dir.join("models.toml");
        if !path.exists() {
            return Err("no model configured (no models.toml)".to_string());
        }
        let configs = load_models(&path).map_err(|e| format!("invalid models.toml: {e}"))?;
        if configs.is_empty() {
            return Err("no model configured (models.toml is empty)".to_string());
        }
        let ids: Vec<_> = configs.iter().map(|c| c.id.clone()).collect();
        let registry = ModelRegistry::new(configs);
        // Phase-1 policy: every mode tries every configured model, in file order,
        // until one connects. (The Phase-7 utility router replaces this.)
        let policy = ModelPolicy::new().with_default_candidates(ids);
        Ok((registry, policy))
    }

    /// The pool-erased [`RunJournal`]: a persist closure (ledger append, with the
    /// run projection updated in step for a `RunStateChanged`) and an
    /// approval-request closure driving the *shared* broker so the runtime's
    /// `await_decision` observes a client's resolution.
    fn journal(&self) -> RunJournal {
        let persist_pool = self.pool.clone();
        let approve_pool = self.pool.clone();
        let approve_broker = self.approvals.clone();
        RunJournal::new(
            move |session: SessionId, actor: Actor, body: EventBody| {
                let pool = persist_pool.clone();
                async move {
                    if let EventBody::RunStateChanged { run_id, state } = &body {
                        projections::set_run_state(&pool, *run_id, *state).await?;
                    }
                    let sequence = ledger::next_sequence(&pool, session).await?;
                    let event = SessionEvent {
                        sequence,
                        occurred_at: Utc::now(),
                        causation_id: None,
                        correlation_id: None,
                        actor,
                        body,
                    };
                    ledger::append_event(&pool, session, &event).await?;
                    Ok(event)
                }
            },
            move |req: ApprovalRequest| {
                let pool = approve_pool.clone();
                let broker = approve_broker.clone();
                async move {
                    let id = broker
                        .request(
                            &pool,
                            req.session_id,
                            req.run_id,
                            req.action,
                            req.risk,
                            req.capabilities,
                            None,
                        )
                        .await?;
                    Ok(id)
                }
            },
        )
    }

    /// The pool-erased [`ArtifactSink`] over the store + pool.
    fn sink(&self, store: ArtifactStore) -> Box<dyn ArtifactSink> {
        let pool = self.pool.clone();
        Box::new(ClosureSink(
            move |media: String, prov: Provenance, bytes: Vec<u8>| {
                let store = store.clone();
                let pool = pool.clone();
                async move {
                    store
                        .put(&pool, &media, DataClassification::Internal, prov, &bytes)
                        .await
                }
            },
        ))
    }

    /// The run body: resolve a model, then drive the agent loop to a terminal
    /// disposition. `Ok(())` means the loop reached a terminal state itself;
    /// `Err(reason)` means the run could not run (e.g. no model configured) and
    /// the caller must fail it cleanly.
    async fn execute(&self, launch: &RunLaunch) -> Result<(), String> {
        let (registry, policy) = self.load_registry()?;
        let resolved = resolve_model(&registry, &policy, launch.mode)
            .await
            .map_err(|e| format!("no model configured: {e}"))?;
        let model_id = resolved.id;
        info!(run_id = %launch.run_id, model = %model_id, "resolved model; executing run");

        let driver = FrameworkModelDriver::from_registry(&registry, model_id)
            .map_err(|e| format!("could not build model client: {e}"))?;

        let runtime = FrameworkAgentRuntime::new(
            registry,
            PolicyEngine::with_defaults(),
            self.approvals.clone(),
            self.subscriptions.clone(),
            self.journal(),
            self.sink(self.artifacts()),
        );
        // Phase 1: the worktree is the repository itself (no per-run worktree
        // allocation yet — STEP 1.8 binds a dedicated worktree later).
        let ctx = RunContext::new(
            launch.session_id,
            launch.run_id,
            launch.objective.clone(),
            launch.mode,
            launch.repository.clone(),
            launch.repository.clone(),
        );
        runtime
            .execute_run(&driver, ctx, CancellationToken::never())
            .await
            .map(|_| ())
            .map_err(|e| format!("run failed: {e}"))
    }
}

impl RunExecutor for RuntimeExecutor {
    fn spawn_run(&self, launch: RunLaunch) {
        let executor = self.clone();
        tokio::spawn(async move {
            // Carry the identity out before `launch` is moved into the worker.
            let session_id = launch.session_id;
            let run_id = launch.run_id;
            let objective = launch.objective.clone();

            // Run the work in a CHILD task so even a panic in the agent loop
            // becomes a clean terminal failure (a `JoinError`) rather than a
            // wedged, forever-`Queued`/`Running` run.
            let worker = executor.clone();
            let joined = tokio::spawn(async move { worker.execute(&launch).await }).await;

            let failure = match joined {
                Ok(Ok(())) => None,              // the loop reached a terminal state itself
                Ok(Err(reason)) => Some(reason), // could not run (e.g. no model)
                Err(join) => Some(format!("run task aborted: {join}")), // panic / cancel
            };

            if let Some(reason) = failure {
                warn!(%run_id, reason = %reason, "run did not execute; failing it cleanly");
                if let Err(e) = recovery::fail_run(
                    &executor.pool,
                    &executor.artifacts(),
                    &executor.subscriptions,
                    run_id,
                    session_id,
                    &objective,
                    &reason,
                )
                .await
                {
                    error!(%run_id, error = %e, "could not fail run cleanly");
                }
            }
        });
    }

    fn collaborators(&self) -> Option<(SubscriptionHub, ApprovalBroker)> {
        Some((self.subscriptions.clone(), self.approvals.clone()))
    }
}
