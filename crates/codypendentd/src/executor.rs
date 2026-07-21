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

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use codypendent_daemon::approvals::ApprovalBroker;
use codypendent_daemon::artifacts::{ArtifactStore, Provenance};
use codypendent_daemon::executor::{RunExecutor, RunLaunch};
use codypendent_daemon::policy::{PolicyEngine, GITHUB_API_ENDPOINT};
use codypendent_daemon::subscriptions::SubscriptionHub;
use codypendent_daemon::{ledger, projections, recovery};
use codypendent_integrations::github::{GitHubApi, RepoId};
use codypendent_knowledge::{assemble_context, extract_candidates, Curation, MemoryStore, Scope};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{Actor, DataClassification, EventBody, RepositoryId, RunId, SessionId};
use codypendent_runtime::agent::{
    cancellation, ApprovalRequest, CancellationHandle, CancellationToken, FrameworkAgentRuntime,
    FrameworkModelDriver, RunContext, RunJournal,
};
use codypendent_runtime::models::{load_models, resolve_model, ModelPolicy, ModelRegistry};
use codypendent_runtime::tools::{ArtifactSink, ClosureSink};
use sqlx::SqlitePool;
use tracing::{error, info, warn};

use crate::scan;
use crate::workflows::{AgentLoopNodeExecutor, WorkflowConductorHost};

/// Executes accepted runs by driving the runtime agent loop. Cheap to clone —
/// every field is an `Arc`-backed handle or a plain (clonable) path bundle.
#[derive(Clone)]
pub struct RuntimeExecutor {
    pool: SqlitePool,
    paths: RuntimePaths,
    subscriptions: SubscriptionHub,
    approvals: ApprovalBroker,
    /// Repositories already folded into the code graph this process's lifetime.
    /// A per-user daemon can serve several checkouts over one socket, so each run
    /// derives its OWN repository identity from its repository root and the first
    /// run for a repository warms it here (issue #6 item 1). Seeded with the
    /// startup repository `main` already scanned, so the primary checkout is never
    /// re-scanned. `Arc<Mutex<…>>` so every clone shares one set.
    scanned: Arc<Mutex<HashSet<RepositoryId>>>,
    /// Live per-run cancellation handles, keyed by `RunId`. `spawn_run` registers
    /// a run's handle before its loop starts and removes it once the loop is
    /// terminal; [`cancel_run`](RunExecutor::cancel_run) fires the matching handle
    /// so an accepted `CancelRun` actually stops the runtime instead of only
    /// marking the projection `Cancelled`. `Arc<Mutex<…>>` so every clone of this
    /// (cheap-to-clone) executor shares one registry — the clone the server holds
    /// must see the handle the worker task registered.
    cancellations: Arc<Mutex<HashMap<RunId, CancellationHandle>>>,
    /// The GitHub client the `github.*` tools call, if a personal-mode token was
    /// discovered at startup (Phase 3 STEP 3.2). `None` leaves those tools
    /// unavailable and the run behaves exactly as before.
    github: Option<Arc<dyn GitHubApi>>,
    /// The workflow-execution host: creates, drives, recovers, and controls durable
    /// workflow runs (Phase 5 STEP 5.2). One shared host backs both the
    /// [`WorkflowStarter`](codypendent_daemon::workflows::WorkflowStarter) and
    /// [`WorkflowLifecycle`](codypendent_daemon::workflows::WorkflowLifecycle) seams
    /// the server pulls out, so their per-run drive locks are the same registry —
    /// a `PauseWorkflow` and the `StartWorkflow` drive it pauses serialize together.
    workflow_host: WorkflowConductorHost<AgentLoopNodeExecutor>,
}

impl RuntimeExecutor {
    /// Build an executor over the daemon's pool + paths, minting the shared
    /// fan-out + approval broker the server binds to via [`Self::collaborators`].
    /// `startup_repository` is the id `main` already scanned from the daemon's own
    /// directory; it seeds the "already scanned" set so the primary checkout is
    /// not re-scanned when its first run arrives.
    pub fn new(pool: SqlitePool, paths: RuntimePaths, startup_repository: RepositoryId) -> Self {
        let subscriptions = SubscriptionHub::new();
        // Bind the broker to the SAME hub the server fans out to, so an
        // `ApprovalRequested` raised by the agent loop reaches attached clients
        // live (not only on re-attach catch-up).
        let approvals = ApprovalBroker::new().with_subscriptions(subscriptions.clone());
        let mut scanned = HashSet::new();
        scanned.insert(startup_repository);
        let workflow_host =
            WorkflowConductorHost::new(pool.clone(), Arc::new(AgentLoopNodeExecutor));
        Self {
            pool,
            paths,
            subscriptions,
            approvals,
            scanned: Arc::new(Mutex::new(scanned)),
            cancellations: Arc::new(Mutex::new(HashMap::new())),
            github: None,
            workflow_host,
        }
    }

    /// Startup recovery for durable workflow runs (Phase 5 STEP 5.2): spawn a drive
    /// for every incomplete run so a crash-interrupted workflow resumes. Called from
    /// `main` alongside [`relaunch_queued_runs`](Self::relaunch_queued_runs); the
    /// drives run in the background, so this returns as soon as they are spawned.
    pub async fn recover_workflows(&self) -> anyhow::Result<usize> {
        Ok(self.workflow_host.recover().await?)
    }

    /// Inject the GitHub client (Phase 3 STEP 3.2). When set, the agent loop
    /// gains the `github.*` tools and the policy admits the GitHub API endpoint
    /// so a mutation reaches the approval gate (every write still needs approval).
    pub fn with_github(mut self, github: Arc<dyn GitHubApi>) -> Self {
        self.github = Some(github);
        self
    }

    /// Warm `repository`'s code graph the first time this daemon serves a run for
    /// it, so [`emit_context`](Self::emit_context) opens with the right repository
    /// map. The lock is released before the (async) scan — a `std` mutex is never
    /// held across an await — and only the first caller for a repository scans;
    /// later runs reuse the graph.
    async fn ensure_scanned(&self, repository: RepositoryId, root: &Path) {
        let newly = {
            let mut seen = self.scanned.lock().expect("scanned set lock");
            seen.insert(repository)
        };
        if newly {
            scan::scan_repository(&self.pool, repository, root).await;
        }
    }

    /// The content-addressed store rooted at `<data_dir>/artifacts`.
    fn artifacts(&self) -> ArtifactStore {
        ArtifactStore::new(self.paths.data_dir.join("artifacts"))
    }

    /// Re-launch every run still `Queued` at startup. A crash between committing
    /// the `StartRun` transaction and the fire-and-forget `spawn_run` leaves a run
    /// `Queued` with no worker; startup recovery only sweeps *live* states and
    /// skips `Queued`, so without this the run is stuck forever. Re-launching is
    /// safe — the agent loop does not re-emit `RunStarted` for an existing run.
    /// Returns how many were re-launched.
    pub async fn relaunch_queued_runs(&self) -> anyhow::Result<usize> {
        let rows: Vec<(String, String, String, String)> =
            sqlx::query_as("SELECT id, session_id, objective, mode FROM runs WHERE state = ?")
                .bind(projections::run_state_to_db(
                    codypendent_protocol::RunState::Queued,
                ))
                .fetch_all(&self.pool)
                .await?;

        let fallback = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let mut relaunched = 0usize;
        for (id, session, objective, mode) in rows {
            let (Ok(run_id), Ok(session_id)) = (
                id.parse::<codypendent_protocol::RunId>(),
                session.parse::<SessionId>(),
            ) else {
                warn!(run = %id, "skipping a queued run with an unparseable id");
                continue;
            };
            // Recover the run's own repository from its originating StartRun
            // command (issue #6 item 1): relaunching against the daemon's cwd
            // would attribute a multi-checkout run's context and memories to the
            // wrong repository. Fall back to the cwd exactly as the live path
            // does for an older client that sent none.
            let repository = queued_run_repository(&self.pool, &id)
                .await
                .unwrap_or_else(|| fallback.clone());
            self.spawn_run(RunLaunch {
                session_id,
                run_id,
                objective,
                mode: projections::agent_mode_from_db(&mode),
                repository,
            });
            relaunched += 1;
        }
        Ok(relaunched)
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
    async fn execute(&self, launch: &RunLaunch, token: CancellationToken) -> Result<(), String> {
        let (registry, policy) = self.load_registry()?;
        let resolved = resolve_model(&registry, &policy, launch.mode)
            .await
            .map_err(|e| format!("no model configured: {e}"))?;
        let model_id = resolved.id;
        info!(run_id = %launch.run_id, model = %model_id, "resolved model; executing run");

        let driver = FrameworkModelDriver::from_registry(&registry, model_id)
            .map_err(|e| format!("could not build model client: {e}"))?;

        // When a GitHub client is configured, admit the GitHub API endpoint on
        // the network allow-list so a mutation reaches the approval gate rather
        // than a hard network deny — every GitHub write still requires approval.
        let policy = if self.github.is_some() {
            PolicyEngine::with_defaults_allowing_network([GITHUB_API_ENDPOINT.to_string()])
        } else {
            PolicyEngine::with_defaults()
        };

        let mut runtime = FrameworkAgentRuntime::new(
            registry,
            policy,
            self.approvals.clone(),
            self.subscriptions.clone(),
            self.journal(),
            self.sink(self.artifacts()),
        );
        if let Some(github) = &self.github {
            runtime = runtime.with_github(github.clone());
        }

        // Phase 1: the worktree is the repository itself (no per-run worktree
        // allocation yet — STEP 1.8 binds a dedicated worktree later).
        let mut ctx = RunContext::new(
            launch.session_id,
            launch.run_id,
            launch.objective.clone(),
            launch.mode,
            launch.repository.clone(),
            launch.repository.clone(),
        );
        // Resolve the run's GitHub `owner/repo` from the checkout's origin remote,
        // so the `github.*` tools know their target. Only meaningful when a client
        // is configured; a checkout with no GitHub origin leaves the tools inert.
        if self.github.is_some() {
            if let Some(repo) = resolve_github_repo(&launch.repository).await {
                ctx = ctx.with_github_repo(repo);
            }
        }

        // Seed the run with the session's latest IDE context (Phase 3 STEP 3.4),
        // so the read path can flag a file whose disk bytes diverge from an unsaved
        // editor buffer. Absent (no attached IDE) leaves the read path unchanged.
        match projections::load_ide_context(&self.pool, launch.session_id).await {
            Ok(Some(ide)) if !ide.dirty_buffers.is_empty() => {
                ctx = ctx.with_ide_context(ide.dirty_buffers);
            }
            Ok(_) => {}
            Err(error) => warn!(%error, "could not load IDE context for the run"),
        }

        runtime
            .execute_run(&driver, ctx, token)
            .await
            .map(|_| ())
            .map_err(|e| format!("run failed: {e}"))
    }

    /// Append a run-scoped `NoteAppended` event to `session_id`'s ledger and
    /// publish it to the shared fan-out — append-then-publish, mirroring
    /// [`recovery::fail_run`] so an attached client observes the note live. Used
    /// to surface the context manifest and the curated memories in a run's trace.
    /// The note carries its `run_id` so a client routes it to the right run's
    /// transcript even when runs interleave (issue #6 item 3).
    async fn emit_note(
        &self,
        session_id: SessionId,
        run_id: RunId,
        text: String,
    ) -> anyhow::Result<()> {
        // Atomic sequence claim — the note may race a concurrent client command
        // on the same session.
        let event = ledger::append_next_event(
            &self.pool,
            session_id,
            &Actor::System,
            &EventBody::NoteAppended {
                text,
                run_id: Some(run_id),
            },
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
    async fn emit_context(
        &self,
        session_id: SessionId,
        run_id: RunId,
        repository: RepositoryId,
        objective: &str,
    ) {
        // System (built-ins) + this repository (harvested run memories are stored
        // at repository visibility), so a memory a prior run curated resurfaces.
        let scopes = [Scope::System, Scope::Repository(repository)];
        match assemble_context(&self.pool, repository, objective, &scopes).await {
            Ok(manifest) => {
                if let Err(error) = self.emit_note(session_id, run_id, manifest.render()).await {
                    warn!(%session_id, %run_id, %error, "could not emit run context note");
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
    async fn harvest_memories(
        &self,
        session_id: SessionId,
        run_id: RunId,
        repository: RepositoryId,
    ) {
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
        let repository_scope = Scope::Repository(repository);
        let mut candidates = extract_candidates(&events, Scope::Session(session_id));
        for candidate in &mut candidates {
            candidate.scope = Some(repository_scope.clone());
        }
        let store = MemoryStore::new();
        for candidate in candidates {
            match store.curate(&self.pool, candidate).await {
                Ok(Curation::Accepted(record)) | Ok(Curation::Superseded { record, .. }) => {
                    if let Err(error) = self
                        .emit_note(
                            session_id,
                            run_id,
                            format!("remembered: {}", record.statement),
                        )
                        .await
                    {
                        warn!(%session_id, %run_id, %error, "could not emit curated-memory note");
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
            // This run's OWN repository identity, derived from its repository root
            // (issue #6 item 1) — NOT the daemon's startup directory — so a shared
            // daemon attributes its context map and curated memories correctly.
            let repository = scan::repository_id_for(&launch.repository);

            // Register a cancellation handle BEFORE the loop starts, so a
            // `CancelRun` accepted at any point after this run was launched can
            // stop it. The token drives `execute`; the handle stays in the shared
            // registry for `cancel_run` to fire.
            let (handle, token) = cancellation();
            executor
                .cancellations
                .lock()
                .expect("cancellations registry lock")
                .insert(run_id, handle);

            // Warm this repository's code graph the first time the daemon serves a
            // run for it, so the context below opens with the right repository map.
            executor
                .ensure_scanned(repository, &launch.repository)
                .await;

            // Open the run's trace with the knowledge-fabric context (repository
            // map + retrieved tool/skill cards + cited memories). Emitted here,
            // BEFORE the agent loop, so the note never races the loop's own
            // sequence allocations.
            executor
                .emit_context(session_id, run_id, repository, &objective)
                .await;

            // Run the work in a CHILD task so even a panic in the agent loop
            // becomes a clean terminal failure (a `JoinError`) rather than a
            // wedged, forever-`Queued`/`Running` run.
            let worker = executor.clone();
            let joined = tokio::spawn(async move { worker.execute(&launch, token).await }).await;

            let failure = match joined {
                Ok(Ok(())) => None,              // the loop reached a terminal state itself
                Ok(Err(reason)) => Some(reason), // could not run (e.g. no model)
                Err(join) => Some(format!("run task aborted: {join}")), // panic / cancel
            };

            if let Some(reason) = failure {
                warn!(%run_id, reason = %reason, "run did not execute; failing it cleanly");
                // Retried: this is the last line of defense against a run being
                // left non-terminal (a headless `codypendent run` then hangs
                // forever), and a transient SQLITE_BUSY from a concurrently
                // streaming run must not defeat it.
                let mut attempt = 0u32;
                loop {
                    attempt += 1;
                    match recovery::fail_run(
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
                        Ok(()) => break,
                        Err(e) if attempt < 4 => {
                            warn!(%run_id, error = %e, attempt, "failing the run did not stick; retrying");
                            tokio::time::sleep(std::time::Duration::from_millis(
                                100 * u64::from(attempt),
                            ))
                            .await;
                        }
                        Err(e) => {
                            error!(%run_id, error = %e, "could not fail run cleanly");
                            break;
                        }
                    }
                }
            }

            // The run has reached a terminal state; drop its cancellation handle
            // so the registry does not grow without bound (and a late `cancel_run`
            // for this run becomes a clean no-op).
            executor
                .cancellations
                .lock()
                .expect("cancellations registry lock")
                .remove(&run_id);

            // The run has now reached a terminal state (either the loop finished
            // it, or `fail_run` above did). Harvest any curated memories from its
            // event trace and note each durable one — emitted AFTER the loop, so
            // these appends never race it either.
            executor
                .harvest_memories(session_id, run_id, repository)
                .await;
        });
    }

    fn cancel_run(&self, run_id: RunId) {
        // Fire the run's cancellation token if it is still executing in this
        // process; a finished or unknown run simply is not in the registry, so
        // this is a clean no-op.
        if let Some(handle) = self
            .cancellations
            .lock()
            .expect("cancellations registry lock")
            .get(&run_id)
        {
            handle.cancel();
        }
    }

    fn collaborators(&self) -> Option<(SubscriptionHub, ApprovalBroker)> {
        Some((self.subscriptions.clone(), self.approvals.clone()))
    }

    fn document_mutator(&self) -> Option<Arc<dyn codypendent_daemon::documents::DocumentMutator>> {
        // Apply `MutateDocument` over the knowledge document engine (mode-gated by
        // scope, single-writer via edit leases). Shares the daemon's pool.
        Some(Arc::new(crate::documents::KnowledgeDocumentMutator::new(
            self.pool.clone(),
        )))
    }

    fn document_leaser(&self) -> Option<Arc<dyn codypendent_daemon::documents::DocumentLeaser>> {
        // Acquire/release the block-range edit leases that gate `MutateDocument`,
        // over the same knowledge lease store the mutator's `require` enforces.
        Some(Arc::new(crate::documents::KnowledgeDocumentMutator::new(
            self.pool.clone(),
        )))
    }

    fn workflow_starter(&self) -> Option<Arc<dyn codypendent_daemon::workflows::WorkflowStarter>> {
        // Create a durable run from a `StartWorkflow` manifest and drive it to a
        // terminal state in the background (Phase 5 STEP 5.2). Shares the one host,
        // so its per-run drive locks match the lifecycle seam's.
        Some(Arc::new(self.workflow_host.clone()))
    }

    fn workflow_lifecycle(
        &self,
    ) -> Option<Arc<dyn codypendent_daemon::workflows::WorkflowLifecycle>> {
        // Pause/resume/retry an existing durable run over the same host (Phase 5
        // STEP 5.2).
        Some(Arc::new(self.workflow_host.clone()))
    }
}

/// The `repository` recorded on the StartRun command that created a queued run,
/// if any. The commands table stores the applied outcome (`result_json`, with
/// `created_run`) beside the body, so the originating command is found by the
/// run id it created.
async fn queued_run_repository(
    pool: &sqlx::SqlitePool,
    run_id: &str,
) -> Option<std::path::PathBuf> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT body FROM commands \
         WHERE status = 'applied' AND json_extract(result_json, '$.created_run') = ?",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await
    .ok()?;
    let (body_json,) = row?;
    let body: codypendent_protocol::CommandBody = serde_json::from_str(&body_json).ok()?;
    match body {
        codypendent_protocol::CommandBody::StartRun { repository, .. } => {
            repository.map(std::path::PathBuf::from)
        }
        _ => None,
    }
}

/// Resolve a checkout's GitHub `owner/repo` from its `origin` remote, or `None`
/// if the checkout has no GitHub origin (the `github.*` tools then stay inert).
async fn resolve_github_repo(repository: &Path) -> Option<RepoId> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["remote", "get-url", "origin"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout);
    parse_github_slug(url.trim())
}

/// Parse an `owner/repo` [`RepoId`] from a GitHub remote URL, accepting both the
/// HTTPS (`https://github.com/owner/repo.git`) and scp-like SSH
/// (`git@github.com:owner/repo.git`) forms. The host is matched **exactly**
/// against `github.com` (never by substring), so `mygithub.com` or
/// `github.com.evil.example` is rejected, and any embedded userinfo (a token in
/// the URL) is discarded, not propagated.
fn parse_github_slug(url: &str) -> Option<RepoId> {
    // Drop the scheme (`https://`, `ssh://`) and any `user[:pass]@` userinfo.
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, rest)| rest);
    // The host runs up to the first delimiter: `/` in the URL form, `:` in the
    // scp-like form. Everything after it is the path.
    let boundary = rest.find(['/', ':'])?;
    let host = &rest[..boundary];
    if host != "github.com" {
        return None;
    }
    let mut path = rest[boundary + 1..].trim_start_matches('/');
    // A URL-form remote may carry an explicit port (`github.com:443/owner/repo`);
    // the `:` boundary would otherwise hand the port digits to the owner slot.
    if rest.as_bytes()[boundary] == b':' {
        if let Some((maybe_port, remainder)) = path.split_once('/') {
            if !maybe_port.is_empty() && maybe_port.bytes().all(|b| b.is_ascii_digit()) {
                path = remainder;
            }
        }
    }
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/').filter(|segment| !segment.is_empty());
    let owner = parts.next()?;
    let repo = parts.next()?;
    Some(RepoId::new(owner.to_string(), repo.to_string()))
}

#[cfg(test)]
mod tests {
    use super::parse_github_slug;

    #[test]
    fn parses_https_and_ssh_remotes() {
        for url in [
            "https://github.com/octocat/hello-world.git",
            "https://github.com/octocat/hello-world",
            "git@github.com:octocat/hello-world.git",
            "ssh://git@github.com/octocat/hello-world.git",
        ] {
            let repo = parse_github_slug(url).expect("parse");
            assert_eq!(repo.owner, "octocat");
            assert_eq!(repo.repo, "hello-world");
        }
    }

    #[test]
    fn discards_url_embedded_credentials() {
        // A token in the URL must be dropped, and the host still matched exactly.
        let repo = parse_github_slug("https://user:ghp_secret@github.com/octocat/hello-world.git")
            .expect("parse");
        assert_eq!(repo.owner, "octocat");
        assert_eq!(repo.repo, "hello-world");
    }

    #[test]
    fn rejects_non_github_and_lookalike_hosts() {
        assert!(parse_github_slug("https://gitlab.com/octocat/hello-world.git").is_none());
        // Look-alike hosts that merely contain the substring must be rejected.
        assert!(parse_github_slug("https://mygithub.com/octocat/hello-world.git").is_none());
        assert!(parse_github_slug("https://github.com.evil.example/octocat/hello.git").is_none());
        assert!(parse_github_slug("").is_none());
    }
}
