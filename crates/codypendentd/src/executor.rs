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
use codypendent_knowledge::{assemble_context, extract_candidates, Curation, MemoryStore, Scope};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{Actor, DataClassification, EventBody, RepositoryId, SessionId};
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
    /// The repository the knowledge fabric attributes this process's runs to.
    /// Minted once at startup (in `main`) and shared with the code-graph scan, so
    /// every run's context maps + memories share one stable repository identity.
    repository: RepositoryId,
}

impl RuntimeExecutor {
    /// Build an executor over the daemon's pool + paths, minting the shared
    /// fan-out + approval broker the server binds to via [`Self::collaborators`].
    /// `repository` is the (process-stable) id `main` also feeds the startup
    /// code-graph scan, so a run's repository map and its curated memories share
    /// one identity.
    pub fn new(pool: SqlitePool, paths: RuntimePaths, repository: RepositoryId) -> Self {
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
            repository,
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
                    // Append first — with an atomic sequence claim, so a live run
                    // and a concurrent client command can never collide on a
                    // sequence — then advance the (derived, replay-rebuildable)
                    // run projection, so an append failure never leaves the
                    // projection ahead of the ledger.
                    let event =
                        ledger::append_next_event(&pool, session, &actor, &body, Utc::now())
                            .await?;
                    if let EventBody::RunStateChanged { run_id, state } = &event.body {
                        projections::set_run_state(&pool, *run_id, *state).await?;
                    }
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

    /// Append a `NoteAppended` event to `session_id`'s ledger and publish it to
    /// the shared fan-out — append-then-publish, mirroring [`recovery::fail_run`]
    /// so an attached client observes the note live. Used to surface the context
    /// manifest and the curated memories in a run's trace.
    async fn emit_note(&self, session_id: SessionId, text: String) -> anyhow::Result<()> {
        // Atomic sequence claim — the note may race a concurrent client command
        // on the same session.
        let event = ledger::append_next_event(
            &self.pool,
            session_id,
            &Actor::System,
            &EventBody::NoteAppended { text },
            Utc::now(),
        )
        .await?;
        // Persist-before-publish: only after the append does the note fan out.
        self.subscriptions.publish(session_id, event);
        Ok(())
    }

    /// Assemble the knowledge-fabric context (repository map + tool/skill cards +
    /// cited memories) for `objective` and note its render into the trace, so
    /// every run opens with the three manifests.
    ///
    /// Called **before** the agent loop, never concurrently with it — the note is
    /// appended and published from the worker before `execute` spawns, so it can
    /// never race the loop for a sequence. A fabric failure is warned and swallowed
    /// (context is an aid, never a gate on running).
    async fn emit_context(&self, session_id: SessionId, objective: &str) {
        // System (built-ins) + this repository (harvested run memories are stored
        // at repository visibility), so a memory a prior run curated resurfaces.
        let scopes = [Scope::System, Scope::Repository(self.repository)];
        match assemble_context(&self.pool, self.repository, objective, &scopes).await {
            Ok(manifest) => {
                if let Err(error) = self.emit_note(session_id, manifest.render()).await {
                    warn!(%session_id, %error, "could not emit run context note");
                }
            }
            Err(error) => warn!(%session_id, %error, "could not assemble run context"),
        }
    }

    /// After a run reaches a terminal state, harvest curated memories from its own
    /// event trace and note each durable one, so "a run produces a curated memory
    /// whose provenance opens to its source" holds for every run.
    ///
    /// Runs **after** `execute` returns (the loop is no longer appending), so the
    /// note appends never race the agent loop. The curator redacts secrets before
    /// anything is stored, so a `remembered:` note can never carry secret text.
    /// Every failure is warned and swallowed — a harvesting error must not turn a
    /// finished run into a failed one.
    async fn harvest_memories(&self, session_id: SessionId) {
        let events = match ledger::load_events(&self.pool, session_id).await {
            Ok(events) => events,
            Err(error) => {
                warn!(%session_id, %error, "could not load events for memory harvest");
                return;
            }
        };
        // Extract under the SESSION scope so the event-range extractors (repeated
        // `shell.run` procedures, explicit `memory.propose:` notes) can resolve
        // their evidence session id — a System scope yields none, harvesting only
        // chronicle memories. Then re-anchor each candidate to REPOSITORY
        // visibility so the curated memory resurfaces in later runs' context
        // (which `emit_context` queries at System + this repository); a
        // session-scoped memory would never be seen again.
        let repository_scope = Scope::Repository(self.repository);
        let mut candidates = extract_candidates(&events, Scope::Session(session_id));
        for candidate in &mut candidates {
            candidate.scope = Some(repository_scope.clone());
        }
        let store = MemoryStore::new();
        for candidate in candidates {
            match store.curate(&self.pool, candidate).await {
                Ok(Curation::Accepted(record)) | Ok(Curation::Superseded { record, .. }) => {
                    if let Err(error) = self
                        .emit_note(session_id, format!("remembered: {}", record.statement))
                        .await
                    {
                        warn!(%session_id, %error, "could not emit curated-memory note");
                    }
                }
                // Redacted / Duplicate / Rejected: nothing durable, nothing to note.
                Ok(_) => {}
                Err(error) => warn!(%session_id, %error, "memory curation failed"),
            }
        }
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

            // Open the run's trace with the knowledge-fabric context (repository
            // map + retrieved tool/skill cards + cited memories). Emitted here,
            // BEFORE the agent loop, so the note never races the loop's own
            // sequence allocations.
            executor.emit_context(session_id, &objective).await;

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

            // The run has now reached a terminal state (either the loop finished
            // it, or `fail_run` above did). Harvest any curated memories from its
            // event trace and note each durable one — emitted AFTER the loop, so
            // these appends never race it either.
            executor.harvest_memories(session_id).await;
        });
    }

    fn collaborators(&self) -> Option<(SubscriptionHub, ApprovalBroker)> {
        Some((self.subscriptions.clone(), self.approvals.clone()))
    }
}
