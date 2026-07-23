//! STEP 1.10 agent-loop integration tests.
//!
//! These exercise the whole [`FrameworkAgentRuntime`] loop with a
//! [`ScriptedDriver`] (no live model, no HTTP), a real temp `db::open_database`
//! pool, a temp git worktree, a real [`ArtifactStore`], [`ApprovalBroker`], and
//! [`SubscriptionHub`]. The loop reaches the daemon's pool through a
//! [`RunJournal`] and a [`ClosureSink`] built by the macros below — both capture
//! a pool *value* whose type this crate cannot name (see the agent-module docs),
//! exactly as the tool layer's `store_sink!` does.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use codypendent_daemon::approvals::ApprovalBroker;
use codypendent_daemon::artifacts::{ArtifactStore, Provenance};
use codypendent_daemon::db::open_database;
use codypendent_daemon::policy::{PolicyEngine, GITHUB_API_ENDPOINT};
use codypendent_daemon::subscriptions::SubscriptionHub;
use codypendent_daemon::{ledger, projections};
use codypendent_integrations::github::{model, GitHubApi, GitHubError, RepoId};
use codypendent_protocol::{
    Actor, AgentMode, ApprovalDecision, ApprovalScope, BlackboardItemView, DataClassification,
    EventBody, ProposedAction, RunDisposition, RunId, RunState, SessionEvent, SessionId,
    ToolOutcome,
};
use codypendent_runtime::agent::{
    cancellation, ApprovalRequest, CancellationToken, FrameworkAgentRuntime, ModelStep, ModelUsage,
    RunContext, RunJournal, ScriptedDriver, StepOutcome, WorkflowContext,
};
use codypendent_runtime::blackboard::{BlackboardChannel, BlackboardChannelError, BlackboardPost};
use codypendent_runtime::models::ModelRegistry;
use codypendent_runtime::tools::{
    ArtifactSink, BlackboardPostTool, BlackboardQueryTool, ClosureSink,
};
use serde_json::json;

/// An [`ArtifactSink`] over a store + pool, capturing clones (the pool's type is
/// unnameable here, so this must be a macro, not a function).
macro_rules! store_sink {
    ($store:expr, $pool:expr) => {{
        let store = $store.clone();
        let pool = $pool.clone();
        ClosureSink(move |media: String, prov: Provenance, bytes: Vec<u8>| {
            let store = store.clone();
            let pool = pool.clone();
            async move {
                store
                    .put(&pool, &media, DataClassification::Internal, prov, &bytes)
                    .await
            }
        })
    }};
}

/// A [`RunJournal`] over the ledger/projections/broker, capturing pool + broker
/// clones. The approval-request closure drives the same broker instance passed
/// to the runtime, so `await_decision` observes the resolution.
macro_rules! run_journal {
    ($pool:expr, $broker:expr) => {{
        let persist_pool = $pool.clone();
        let approve_pool = $pool.clone();
        let approve_broker = $broker.clone();
        RunJournal::new(
            move |session: SessionId, actor: Actor, body: EventBody| {
                let pool = persist_pool.clone();
                async move {
                    // A RunStateChanged updates the run projection in step with
                    // its ledger append (STEP 1.10 rule 1).
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
    }};
}

/// Assemble a runtime over the given collaborators (empty model registry — the
/// scripted driver needs none — and the built-in policy defaults).
macro_rules! build_runtime {
    ($pool:expr, $store:expr, $broker:expr, $hub:expr) => {{
        let journal = run_journal!($pool, $broker);
        let sink: Box<dyn ArtifactSink> = Box::new(store_sink!($store, $pool));
        FrameworkAgentRuntime::new(
            ModelRegistry::new(Vec::new()),
            PolicyEngine::with_defaults(),
            $broker.clone(),
            $hub.clone(),
            journal,
            sink,
        )
    }};
}

/// Seed a run row *and* its `RunStarted` event — exactly as the `StartRun`
/// command (STEP 1.3, `commands::apply_start_run`) does before the agent loop
/// runs. `execute_run` then executes an *already-started* run and must add zero
/// further `RunStarted`s. (A macro, not a fn: the pool's type is unnameable
/// here, like `store_sink!`.)
macro_rules! seed_started_run {
    ($pool:expr, $session:expr, $run:expr, $objective:expr, $mode:expr) => {{
        projections::insert_run(&$pool, $run, $session, $objective, $mode, "hosted", "{}")
            .await
            .unwrap();
        let started = SessionEvent {
            sequence: ledger::next_sequence(&$pool, $session).await.unwrap(),
            occurred_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body: EventBody::RunStarted {
                run_id: $run,
                objective: $objective.to_string(),
                mode: $mode,
            },
        };
        ledger::append_event(&$pool, $session, &started)
            .await
            .unwrap();
    }};
}

// --- git worktree helpers (mirrors tools_it.rs) ----------------------------

async fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .expect("git runs")
}

async fn init_repo(dir: &Path) {
    assert!(git(dir, &["init", "-q"]).await.status.success());
    git(dir, &["config", "user.email", "t@example.com"]).await;
    git(dir, &["config", "user.name", "Test"]).await;
    git(dir, &["config", "commit.gpgsign", "false"]).await;
}

/// A committed repo with a tracked file at `file`.
async fn repo_with_committed_file(dir: &Path, file: &str, contents: &str) {
    init_repo(dir).await;
    std::fs::write(dir.join(file), contents).unwrap();
    assert!(git(dir, &["add", "."]).await.status.success());
    assert!(git(dir, &["commit", "-q", "-m", "init"])
        .await
        .status
        .success());
}

/// A short label for an event body, for ordering assertions.
fn label(body: &EventBody) -> &'static str {
    match body {
        EventBody::RunStarted { .. } => "RunStarted",
        EventBody::RunStateChanged { .. } => "RunStateChanged",
        EventBody::ModelStreamDelta { .. } => "ModelStreamDelta",
        EventBody::ToolProposed { .. } => "ToolProposed",
        EventBody::ToolStarted { .. } => "ToolStarted",
        EventBody::ToolCompleted { .. } => "ToolCompleted",
        EventBody::PatchProposed { .. } => "PatchProposed",
        EventBody::SteeringApplied { .. } => "SteeringApplied",
        EventBody::RunCompleted { .. } => "RunCompleted",
        _ => "Other",
    }
}

/// Index of the first event whose label is `want`.
fn index_of(labels: &[&str], want: &str) -> Option<usize> {
    labels.iter().position(|l| *l == want)
}

// ---------------------------------------------------------------------------
// End-to-end fixture run: tools, approval, artifacts, change-set, chronicle.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn end_to_end_run_emits_full_event_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    repo_with_committed_file(&repo, "a.txt", "hello\n").await;
    // Dirty the worktree so the review node produces a change-set.
    std::fs::write(repo.join("a.txt"), "goodbye\n").unwrap();

    let pool = open_database(&dir.path().join("db.sqlite")).await.unwrap();
    let store = ArtifactStore::new(dir.path().join("artifacts"));
    let broker = ApprovalBroker::new();
    let hub = SubscriptionHub::new();

    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(&pool, session, "agent-it")
        .await
        .unwrap();
    // Seed the run + its `RunStarted` the way the StartRun command does, before
    // the loop runs. The loop must not emit a second `RunStarted`.
    seed_started_run!(pool, session, run, "diagnose", AgentMode::Build);

    let runtime = build_runtime!(pool, store, broker, hub);

    // Subscribe AFTER the run was started (so the seeded `RunStarted` is not in
    // the published stream) but before the loop runs, so no loop event is missed.
    let mut rx = hub.subscribe(session);

    let driver = ScriptedDriver::new(vec![
        ModelStep::Say("Inspecting the repository.".to_string()),
        ModelStep::CallTool {
            tool: "shell.run".to_string(),
            args: json!({"program": "git", "args": ["--version"]}),
        },
        ModelStep::CallTool {
            tool: "git.diff".to_string(),
            args: json!({}),
        },
        ModelStep::Finish {
            summary: "diagnosed".to_string(),
        },
    ]);

    let ctx = RunContext::new(
        session,
        run,
        "diagnose",
        AgentMode::Build,
        repo.clone(),
        repo.clone(),
    );
    let handle = tokio::spawn(async move {
        runtime
            .execute_run(&driver, ctx, CancellationToken::never())
            .await
    });

    // Pump published events; auto-resolve every approval as it flows.
    let mut events: Vec<SessionEvent> = Vec::new();
    loop {
        let event = rx.recv().await.expect("event");
        let done = matches!(event.body, EventBody::RunCompleted { .. });
        if let EventBody::ToolProposed { approval_id, .. } = &event.body {
            let approval_id = *approval_id;
            broker
                .resolve(
                    &pool,
                    approval_id,
                    ApprovalDecision::Approve,
                    ApprovalScope::Once,
                    "tester".to_string(),
                )
                .await
                .unwrap();
        }
        events.push(event);
        if done {
            break;
        }
    }
    let disposition = handle.await.unwrap().unwrap().disposition;

    // The run completed.
    assert!(matches!(disposition, RunDisposition::Completed { .. }));
    assert_eq!(
        projections::load_run_state(&pool, run).await.unwrap(),
        Some(RunState::Completed)
    );

    // Build the ordered label list from the *published* stream.
    let mut labels: Vec<&str> = Vec::new();
    for e in &events {
        labels.push(label(&e.body));
    }

    // Key events, in the required relative order. The loop no longer emits
    // `RunStarted` (the StartRun command did, and it was seeded before we
    // subscribed), so the published stream opens on the first state transition.
    assert!(
        matches!(
            events.first().map(|e| &e.body),
            Some(EventBody::RunStateChanged {
                state: RunState::Preparing,
                ..
            })
        ),
        "the loop's first published event is the Preparing transition, not a second RunStarted"
    );
    assert!(
        !labels.contains(&"RunStarted"),
        "execute_run must not emit a RunStarted"
    );
    let running = events
        .iter()
        .position(|e| {
            matches!(
                &e.body,
                EventBody::RunStateChanged {
                    state: RunState::Running,
                    ..
                }
            )
        })
        .expect("Running transition published");
    let tool_proposed = index_of(&labels, "ToolProposed").expect("an approval flowed");
    let tool_started = index_of(&labels, "ToolStarted").unwrap();
    let tool_completed = index_of(&labels, "ToolCompleted").unwrap();
    let patch = index_of(&labels, "PatchProposed").expect("review node proposed a change-set");
    let completed = index_of(&labels, "RunCompleted").unwrap();
    assert!(index_of(&labels, "ModelStreamDelta").is_some());
    assert!(running < tool_proposed);
    assert!(tool_proposed < tool_started);
    assert!(tool_started < tool_completed);
    assert!(tool_completed < patch);
    assert!(patch < completed);

    // Tool output artifacts were created (git --version stdout spilled).
    assert!(events.iter().any(|e| matches!(
        &e.body,
        EventBody::ToolCompleted {
            artifact: Some(_),
            ..
        }
    )));

    // The chronicle artifact is referenced by RunCompleted and is intact JSON.
    let chronicle = events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::RunCompleted { chronicle, .. } => Some(chronicle.clone()),
            _ => None,
        })
        .expect("RunCompleted carries a chronicle");
    assert_eq!(chronicle.media_type, "application/json");
    assert!(store.verify(&pool, chronicle.id).await.unwrap());

    // Every published event is also durably in the ledger.
    let ledger_events = ledger::load_events(&pool, session).await.unwrap();
    assert!(ledger_events
        .iter()
        .any(|e| matches!(e.body, EventBody::RunCompleted { .. })));

    // Exactly ONE RunStarted exists in the ledger — the one the StartRun command
    // seeded — and it precedes the loop's first Running transition. `execute_run`
    // added zero.
    let run_started_count = ledger_events
        .iter()
        .filter(|e| matches!(e.body, EventBody::RunStarted { .. }))
        .count();
    assert_eq!(
        run_started_count, 1,
        "execute_run must not append a second RunStarted"
    );
    let run_started_seq = ledger_events
        .iter()
        .find(|e| matches!(e.body, EventBody::RunStarted { .. }))
        .unwrap()
        .sequence;
    let running_seq = ledger_events
        .iter()
        .find(|e| {
            matches!(
                &e.body,
                EventBody::RunStateChanged {
                    state: RunState::Running,
                    ..
                }
            )
        })
        .unwrap()
        .sequence;
    assert!(
        run_started_seq < running_seq,
        "the seeded RunStarted precedes the loop's Running transition"
    );
}

// ---------------------------------------------------------------------------
// A client disconnect (no subscriber) must not stop the run.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_disconnect_does_not_stop_run() {
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(repo.join("code.rs"), "fn main() {}\n").unwrap();

    let pool = open_database(&dir.path().join("db.sqlite")).await.unwrap();
    let store = ArtifactStore::new(dir.path().join("artifacts"));
    let broker = ApprovalBroker::new();
    let hub = SubscriptionHub::new();

    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(&pool, session, "no-client")
        .await
        .unwrap();
    // Seed the run + its `RunStarted` as the StartRun command does.
    seed_started_run!(pool, session, run, "read", AgentMode::Explore);

    let runtime = build_runtime!(pool, store, broker, hub);

    // Read-only Explore run: no approval, no subscriber at all.
    let driver = ScriptedDriver::new(vec![
        ModelStep::Say("reading".to_string()),
        ModelStep::CallTool {
            tool: "workspace.read_file".to_string(),
            args: json!({"path": repo.join("code.rs").to_string_lossy()}),
        },
        ModelStep::Finish {
            summary: "read".to_string(),
        },
    ]);
    let ctx = RunContext::new(
        session,
        run,
        "read",
        AgentMode::Explore,
        repo.clone(),
        repo.clone(),
    );

    // Nobody is subscribed; publishing to zero subscribers is normal.
    let disposition = runtime
        .execute_run(&driver, ctx, CancellationToken::never())
        .await
        .unwrap()
        .disposition;

    assert!(matches!(disposition, RunDisposition::Completed { .. }));
    assert_eq!(
        projections::load_run_state(&pool, run).await.unwrap(),
        Some(RunState::Completed)
    );

    // All events are durably in the ledger despite there being no client.
    let events = ledger::load_events(&pool, session).await.unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e.body, EventBody::RunStarted { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(&e.body, EventBody::ToolCompleted { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e.body, EventBody::RunCompleted { .. })));
}

// ---------------------------------------------------------------------------
// The loop executes an already-started run: it must NOT emit a second
// RunStarted (the StartRun command already appended one).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn execute_run_does_not_emit_a_second_run_started() {
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(repo.join("code.rs"), "fn main() {}\n").unwrap();

    let pool = open_database(&dir.path().join("db.sqlite")).await.unwrap();
    let store = ArtifactStore::new(dir.path().join("artifacts"));
    let broker = ApprovalBroker::new();
    let hub = SubscriptionHub::new();

    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(&pool, session, "single-start")
        .await
        .unwrap();
    // The StartRun command seeds exactly one RunStarted before the loop runs.
    seed_started_run!(pool, session, run, "read", AgentMode::Explore);

    let runtime = build_runtime!(pool, store, broker, hub);

    // A trivial run that finishes immediately (read-only, no tools).
    let driver = ScriptedDriver::new(vec![ModelStep::Finish {
        summary: "done".to_string(),
    }]);
    let ctx = RunContext::new(
        session,
        run,
        "read",
        AgentMode::Explore,
        repo.clone(),
        repo.clone(),
    );

    runtime
        .execute_run(&driver, ctx, CancellationToken::never())
        .await
        .unwrap();

    // The ledger holds exactly one RunStarted (the seeded one); the loop added
    // zero. It still drove the first RunStateChanged (→ Preparing/Running).
    let events = ledger::load_events(&pool, session).await.unwrap();
    let run_started = events
        .iter()
        .filter(|e| matches!(e.body, EventBody::RunStarted { .. }))
        .count();
    assert_eq!(
        run_started, 1,
        "execute_run must not append a second RunStarted"
    );
    assert!(
        events.iter().any(|e| matches!(
            &e.body,
            EventBody::RunStateChanged {
                state: RunState::Running,
                ..
            }
        )),
        "execute_run still drives the first RunStateChanged"
    );
}

// ---------------------------------------------------------------------------
// Phase 7 usage telemetry: measured per-request usage aggregates into the run
// outcome; an all-unmeasured run stays honestly `None` (never a fabricated zero).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn execute_run_aggregates_measured_usage_and_leaves_unmeasured_none() {
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();

    let pool = open_database(&dir.path().join("db.sqlite")).await.unwrap();
    let store = ArtifactStore::new(dir.path().join("artifacts"));
    let broker = ApprovalBroker::new();
    let hub = SubscriptionHub::new();
    let runtime = build_runtime!(pool, store, broker, hub);

    // A driver that reports MEASURED usage for every request. The run makes two
    // requests (Say then Finish), so the outcome carries their saturating sum —
    // the telemetry the `ModelRequestTrace` logs per request and the cost budget
    // charges per node.
    let per_request = ModelUsage {
        prompt_tokens: 10,
        completion_tokens: 5,
        cost_micros: Some(1_000),
    };
    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(&pool, session, "usage")
        .await
        .unwrap();
    seed_started_run!(pool, session, run, "read", AgentMode::Explore);
    let driver = ScriptedDriver::new(vec![
        ModelStep::Say("looking".to_string()),
        ModelStep::Finish {
            summary: "done".to_string(),
        },
    ])
    .with_usage(per_request);
    let ctx = RunContext::new(
        session,
        run,
        "read",
        AgentMode::Explore,
        repo.clone(),
        repo.clone(),
    );
    let outcome = runtime
        .execute_run(&driver, ctx, CancellationToken::never())
        .await
        .unwrap();
    assert_eq!(
        outcome.usage,
        Some(ModelUsage {
            prompt_tokens: 20,
            completion_tokens: 10,
            cost_micros: Some(2_000),
        }),
        "two measured requests sum into the run's aggregated usage"
    );

    // A plain driver reports NO usage: the run stays honestly UNMEASURED (`None`),
    // never a fabricated zero — an all-unmeasured run behaves exactly as before.
    let session2 = SessionId::new();
    let run2 = RunId::new();
    ledger::create_session(&pool, session2, "usage-none")
        .await
        .unwrap();
    seed_started_run!(pool, session2, run2, "read", AgentMode::Explore);
    let plain = ScriptedDriver::new(vec![ModelStep::Finish {
        summary: "done".to_string(),
    }]);
    let ctx2 = RunContext::new(
        session2,
        run2,
        "read",
        AgentMode::Explore,
        repo.clone(),
        repo.clone(),
    );
    let outcome2 = runtime
        .execute_run(&plain, ctx2, CancellationToken::never())
        .await
        .unwrap();
    assert_eq!(
        outcome2.usage, None,
        "an all-unmeasured run reports no usage (never a fabricated zero)"
    );
}

// ---------------------------------------------------------------------------
// Explore mode cannot write: a patch proposal is denied by policy, run continues.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn explore_mode_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    repo_with_committed_file(&repo, "a.txt", "hello\n").await;

    let pool = open_database(&dir.path().join("db.sqlite")).await.unwrap();
    let store = ArtifactStore::new(dir.path().join("artifacts"));
    let broker = ApprovalBroker::new();
    let hub = SubscriptionHub::new();

    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(&pool, session, "explore")
        .await
        .unwrap();
    projections::insert_run(
        &pool,
        run,
        session,
        "look",
        AgentMode::Explore,
        "hosted",
        "{}",
    )
    .await
    .unwrap();

    let runtime = build_runtime!(pool, store, broker, hub);
    let mut rx = hub.subscribe(session);

    // A patch that would apply cleanly if executed — proving it is *policy*,
    // not patch validity, that blocks it.
    let patch = "\
diff --git a/a.txt b/a.txt
index 0000000..1111111 100644
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-hello
+HACKED
";
    let driver = ScriptedDriver::new(vec![
        ModelStep::CallTool {
            tool: "git.apply_patch".to_string(),
            args: json!({"patch": patch}),
        },
        ModelStep::Finish {
            summary: "explored".to_string(),
        },
    ]);
    let ctx = RunContext::new(
        session,
        run,
        "look",
        AgentMode::Explore,
        repo.clone(),
        repo.clone(),
    );

    let disposition = runtime
        .execute_run(&driver, ctx, CancellationToken::never())
        .await
        .unwrap()
        .disposition;

    // The run still reaches Completed — a denial is not a run failure.
    assert!(matches!(disposition, RunDisposition::Completed { .. }));

    // The write proposal produced a policy-denial ToolCompleted (no approval was
    // requested — an Explore write is denied outright), and nothing was written.
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    let denied = events.iter().find_map(|e| match &e.body {
        EventBody::ToolCompleted {
            tool,
            outcome: codypendent_protocol::ToolOutcome::Failed { message },
            ..
        } if tool == "git.apply_patch" => Some(message.clone()),
        _ => None,
    });
    let message = denied.expect("apply_patch was completed as a failure");
    assert!(
        message.contains("policy denied"),
        "expected a policy denial, got: {message}"
    );
    // No approval was ever proposed for a policy-denied action.
    assert!(!events
        .iter()
        .any(|e| matches!(e.body, EventBody::ToolProposed { .. })));
    // The write did not happen.
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "hello\n"
    );
}

// ---------------------------------------------------------------------------
// Steering queued mid-run is applied at a safe point, in event order.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn steering_applied_at_a_safe_point() {
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(repo.join("code.rs"), "fn main() {}\n").unwrap();

    let pool = open_database(&dir.path().join("db.sqlite")).await.unwrap();
    let store = ArtifactStore::new(dir.path().join("artifacts"));
    let broker = ApprovalBroker::new();
    let hub = SubscriptionHub::new();

    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(&pool, session, "steer")
        .await
        .unwrap();
    projections::insert_run(
        &pool,
        run,
        session,
        "read",
        AgentMode::Explore,
        "hosted",
        "{}",
    )
    .await
    .unwrap();

    let runtime = build_runtime!(pool, store, broker, hub);
    let mut rx = hub.subscribe(session);

    // Queue steering before the run so it is applied at the first safe point.
    let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel();
    steer_tx
        .send("focus on the entrypoint".to_string())
        .unwrap();

    let driver = ScriptedDriver::new(vec![
        ModelStep::CallTool {
            tool: "workspace.read_file".to_string(),
            args: json!({"path": repo.join("code.rs").to_string_lossy()}),
        },
        ModelStep::Finish {
            summary: "read".to_string(),
        },
    ]);
    let ctx = RunContext::new(
        session,
        run,
        "read",
        AgentMode::Explore,
        repo.clone(),
        repo.clone(),
    )
    .with_steering(steer_rx);

    let disposition = runtime
        .execute_run(&driver, ctx, CancellationToken::never())
        .await
        .unwrap()
        .disposition;
    assert!(matches!(disposition, RunDisposition::Completed { .. }));

    let mut labels: Vec<&str> = Vec::new();
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    for e in &events {
        labels.push(label(&e.body));
    }

    let steering = index_of(&labels, "SteeringApplied").expect("SteeringApplied appears");
    // Applied at a node boundary (before the tool node), never interleaved
    // inside a ToolStarted/ToolCompleted pair.
    let tool_started = index_of(&labels, "ToolStarted").unwrap();
    let tool_completed = index_of(&labels, "ToolCompleted").unwrap();
    assert!(
        steering < tool_started || steering > tool_completed,
        "steering must land at a safe point, not mid tool call"
    );
    assert!(index_of(&labels, "RunCompleted").is_some());
}

// ---------------------------------------------------------------------------
// Cancellation: a cancelled token stops new work and reaches Cancelled.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancellation_reaches_cancelled_without_running_tools() {
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(repo.join("code.rs"), "fn main() {}\n").unwrap();

    let pool = open_database(&dir.path().join("db.sqlite")).await.unwrap();
    let store = ArtifactStore::new(dir.path().join("artifacts"));
    let broker = ApprovalBroker::new();
    let hub = SubscriptionHub::new();

    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(&pool, session, "cancel")
        .await
        .unwrap();
    projections::insert_run(
        &pool,
        run,
        session,
        "read",
        AgentMode::Explore,
        "hosted",
        "{}",
    )
    .await
    .unwrap();

    let runtime = build_runtime!(pool, store, broker, hub);

    let (handle, token) = cancellation();
    handle.cancel(); // cancel before any work begins

    let driver = ScriptedDriver::new(vec![
        ModelStep::CallTool {
            tool: "workspace.read_file".to_string(),
            args: json!({"path": repo.join("code.rs").to_string_lossy()}),
        },
        ModelStep::Finish {
            summary: "unreached".to_string(),
        },
    ]);
    let ctx = RunContext::new(
        session,
        run,
        "read",
        AgentMode::Explore,
        repo.clone(),
        repo.clone(),
    );

    let disposition = runtime
        .execute_run(&driver, ctx, token)
        .await
        .unwrap()
        .disposition;
    assert!(matches!(disposition, RunDisposition::Cancelled { .. }));
    assert_eq!(
        projections::load_run_state(&pool, run).await.unwrap(),
        Some(RunState::Cancelled)
    );

    // No tool ran; the run reached a terminal chronicle-bearing state.
    let events = ledger::load_events(&pool, session).await.unwrap();
    assert!(!events
        .iter()
        .any(|e| matches!(e.body, EventBody::ToolStarted { .. })));
    assert!(events.iter().any(|e| matches!(
        &e.body,
        EventBody::RunCompleted {
            disposition: RunDisposition::Cancelled { .. },
            ..
        }
    )));
}

// ---------------------------------------------------------------------------
// Cancellation while parked on an approval: the run stops promptly, does not
// execute the tool, and reaches Cancelled even though the approval is never
// resolved.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancellation_while_parked_on_approval_reaches_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    repo_with_committed_file(&repo, "a.txt", "hello\n").await;

    let pool = open_database(&dir.path().join("db.sqlite")).await.unwrap();
    let store = ArtifactStore::new(dir.path().join("artifacts"));
    let broker = ApprovalBroker::new();
    let hub = SubscriptionHub::new();

    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(&pool, session, "cancel-parked")
        .await
        .unwrap();
    projections::insert_run(
        &pool,
        run,
        session,
        "diagnose",
        AgentMode::Build,
        "hosted",
        "{}",
    )
    .await
    .unwrap();

    let runtime = build_runtime!(pool, store, broker, hub);
    let mut rx = hub.subscribe(session);

    // A Build-mode shell.run parks on approval — which we deliberately never
    // resolve, so only cancellation can free the parked run.
    let driver = ScriptedDriver::new(vec![
        ModelStep::CallTool {
            tool: "shell.run".to_string(),
            args: json!({"program": "git", "args": ["--version"]}),
        },
        ModelStep::Finish {
            summary: "unreached".to_string(),
        },
    ]);
    let ctx = RunContext::new(
        session,
        run,
        "diagnose",
        AgentMode::Build,
        repo.clone(),
        repo.clone(),
    );

    let (handle, token) = cancellation();
    let run_task = tokio::spawn(async move { runtime.execute_run(&driver, ctx, token).await });

    // Wait until the run is parked (ToolProposed published), then cancel.
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("an event arrives within 5s")
            .expect("event");
        if matches!(event.body, EventBody::ToolProposed { .. }) {
            break;
        }
    }
    handle.cancel();

    let disposition = tokio::time::timeout(std::time::Duration::from_secs(5), run_task)
        .await
        .expect("the run terminates promptly after cancellation")
        .unwrap()
        .unwrap()
        .disposition;
    assert!(matches!(disposition, RunDisposition::Cancelled { .. }));
    assert_eq!(
        projections::load_run_state(&pool, run).await.unwrap(),
        Some(RunState::Cancelled)
    );

    // The tool never executed — cancellation won the approval race.
    let events = ledger::load_events(&pool, session).await.unwrap();
    assert!(!events
        .iter()
        .any(|e| matches!(e.body, EventBody::ToolStarted { .. })));
    assert!(events.iter().any(|e| matches!(
        &e.body,
        EventBody::RunCompleted {
            disposition: RunDisposition::Cancelled { .. },
            ..
        }
    )));
}

// --- Phase 3 STEP 3.2: GitHub tools in the agent loop ----------------------

/// A fake [`GitHubApi`] that records draft-PR creations and returns a canned
/// pull request, so a run can exercise the write path with no HTTP.
#[derive(Default)]
struct RecordingGitHub {
    /// The `head->base` of every draft-PR create, for assertions.
    created: Arc<Mutex<Vec<String>>>,
    /// The PR numbers passed to `update_pull_request`.
    updated: Arc<Mutex<Vec<u64>>>,
    /// The names passed to `create_check_run_summary`.
    summaries: Arc<Mutex<Vec<String>>>,
}

fn sample_pull_request(number: u64) -> model::PullRequest {
    model::PullRequest {
        number,
        title: "Fix CI".to_string(),
        body: None,
        state: "open".to_string(),
        draft: true,
        html_url: format!("https://github.com/octocat/hello-world/pull/{number}"),
        head: None,
        base: None,
    }
}

fn unused_github_error() -> GitHubError {
    GitHubError::Api {
        status: 501,
        message: "not used in this test".to_string(),
    }
}

#[async_trait]
impl GitHubApi for RecordingGitHub {
    async fn get_pull_request(
        &self,
        _repo: &RepoId,
        number: u64,
    ) -> Result<model::PullRequest, GitHubError> {
        Ok(sample_pull_request(number))
    }

    async fn list_check_runs(
        &self,
        _repo: &RepoId,
        _git_ref: &str,
    ) -> Result<Vec<model::CheckRun>, GitHubError> {
        Ok(Vec::new())
    }

    async fn download_job_logs(
        &self,
        _repo: &RepoId,
        _job_id: u64,
    ) -> Result<Vec<u8>, GitHubError> {
        Ok(Vec::new())
    }

    async fn list_review_comments(
        &self,
        _repo: &RepoId,
        _number: u64,
    ) -> Result<Vec<model::ReviewComment>, GitHubError> {
        Ok(Vec::new())
    }

    async fn create_review_comment(
        &self,
        _repo: &RepoId,
        _number: u64,
        _body: &str,
        _idempotency_key: &str,
    ) -> Result<model::ReviewComment, GitHubError> {
        Err(unused_github_error())
    }

    async fn create_draft_pull_request(
        &self,
        _repo: &RepoId,
        req: &model::NewPullRequest,
        _idempotency_key: &str,
    ) -> Result<model::PullRequest, GitHubError> {
        self.created
            .lock()
            .unwrap()
            .push(format!("{}->{}", req.head, req.base));
        Ok(sample_pull_request(42))
    }

    async fn update_pull_request(
        &self,
        _repo: &RepoId,
        number: u64,
        _req: &model::UpdatePullRequest,
    ) -> Result<model::PullRequest, GitHubError> {
        self.updated.lock().unwrap().push(number);
        Ok(sample_pull_request(number))
    }

    async fn create_check_run_summary(
        &self,
        _repo: &RepoId,
        req: &model::NewCheckRun,
        idempotency_key: &str,
    ) -> Result<model::CheckRun, GitHubError> {
        self.summaries.lock().unwrap().push(req.name.clone());
        Ok(model::CheckRun {
            id: 1,
            name: req.name.clone(),
            status: "completed".to_string(),
            conclusion: req.conclusion.clone(),
            external_id: Some(idempotency_key.to_string()),
        })
    }
}

/// Drive a run whose only tool call is `github.create_draft_pull_request`, under
/// `policy`, resolving any parked approval with `decision`. Returns the temp dir
/// (kept alive so the DB survives), the pool, the run id, the published events,
/// and the recorded `head->base` creates.
async fn run_github_write(
    policy: PolicyEngine,
    decision: ApprovalDecision,
) -> (
    tempfile::TempDir,
    sqlx::SqlitePool,
    RunId,
    Vec<SessionEvent>,
    Vec<String>,
) {
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    let pool = open_database(&dir.path().join("db.sqlite")).await.unwrap();
    let store = ArtifactStore::new(dir.path().join("artifacts"));
    let broker = ApprovalBroker::new();
    let hub = SubscriptionHub::new();

    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(&pool, session, "gh-it")
        .await
        .unwrap();
    seed_started_run!(pool, session, run, "fix ci", AgentMode::Build);

    let recording = Arc::new(RecordingGitHub::default());
    let created = recording.created.clone();

    let runtime = {
        let journal = run_journal!(pool, broker);
        let sink: Box<dyn ArtifactSink> = Box::new(store_sink!(store, pool));
        FrameworkAgentRuntime::new(
            ModelRegistry::new(Vec::new()),
            policy,
            broker.clone(),
            hub.clone(),
            journal,
            sink,
        )
        .with_github(recording as Arc<dyn GitHubApi>)
    };

    let mut rx = hub.subscribe(session);
    let driver = ScriptedDriver::new(vec![
        ModelStep::CallTool {
            tool: "github.create_draft_pull_request".to_string(),
            args: json!({"title": "Fix CI", "head": "fix/ci", "base": "main"}),
        },
        ModelStep::Finish {
            summary: "done".to_string(),
        },
    ]);
    let ctx = RunContext::new(session, run, "fix ci", AgentMode::Build, repo.clone(), repo)
        .with_github_repo(RepoId::new("octocat", "hello-world"));

    let handle = tokio::spawn(async move {
        runtime
            .execute_run(&driver, ctx, CancellationToken::never())
            .await
    });

    let mut events = Vec::new();
    loop {
        let event = rx.recv().await.expect("event");
        let done = matches!(event.body, EventBody::RunCompleted { .. });
        // Resolve any parked approval so the loop never blocks. The deny path
        // emits no `ToolProposed`, so this simply never fires there.
        if let EventBody::ToolProposed { approval_id, .. } = &event.body {
            broker
                .resolve(
                    &pool,
                    *approval_id,
                    decision,
                    ApprovalScope::Once,
                    "tester".to_string(),
                )
                .await
                .unwrap();
        }
        events.push(event);
        if done {
            break;
        }
    }
    handle.await.unwrap().unwrap();
    let created = created.lock().unwrap().clone();
    (dir, pool, run, events, created)
}

/// The proposed action carried by the first `ToolProposed`, if any.
fn first_proposed_action(events: &[SessionEvent]) -> Option<&ProposedAction> {
    events.iter().find_map(|e| match &e.body {
        EventBody::ToolProposed { action, .. } => Some(action),
        _ => None,
    })
}

fn has_failed_tool(events: &[SessionEvent]) -> bool {
    events.iter().any(|e| {
        matches!(
            &e.body,
            EventBody::ToolCompleted {
                outcome: codypendent_protocol::ToolOutcome::Failed { .. },
                ..
            }
        )
    })
}

#[tokio::test]
async fn github_write_parks_for_approval_then_writes() {
    let policy = PolicyEngine::with_defaults_allowing_network([GITHUB_API_ENDPOINT.to_string()]);
    let (_dir, pool, run, events, created) =
        run_github_write(policy, ApprovalDecision::Approve).await;

    // The write proposed a GitHubMutation and parked for approval...
    assert!(
        matches!(
            first_proposed_action(&events),
            Some(ProposedAction::GitHubMutation { .. })
        ),
        "expected a GitHubMutation ToolProposed"
    );
    // ...and only after approval did the client actually create the PR, exactly
    // once, for the requested branch pair.
    assert_eq!(created, vec!["fix/ci->main".to_string()]);
    assert!(events.iter().any(|e| matches!(
        &e.body,
        EventBody::RunCompleted {
            disposition: RunDisposition::Completed { .. },
            ..
        }
    )));

    // Every GitHub write has a matching, durable approval record naming the
    // GitHubMutation action.
    let (action_json,): (String,) =
        sqlx::query_as("SELECT action_json FROM approvals WHERE run_id = ?")
            .bind(run.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(action_json.contains("GitHubMutation"), "{action_json}");
}

#[tokio::test]
async fn github_write_rejected_is_not_performed() {
    let policy = PolicyEngine::with_defaults_allowing_network([GITHUB_API_ENDPOINT.to_string()]);
    let (_dir, _pool, _run, events, created) =
        run_github_write(policy, ApprovalDecision::Reject).await;

    // It parked (a GitHubMutation was proposed) but, rejected, never wrote.
    assert!(matches!(
        first_proposed_action(&events),
        Some(ProposedAction::GitHubMutation { .. })
    ));
    assert!(created.is_empty(), "a rejected write must not call GitHub");
    assert!(has_failed_tool(&events));
}

#[tokio::test]
async fn github_write_denied_without_network_allow() {
    // The default policy has an empty network allow-list, so a GitHub mutation
    // is denied before it can ever reach the approval gate.
    let (_dir, _pool, _run, events, created) =
        run_github_write(PolicyEngine::with_defaults(), ApprovalDecision::Approve).await;

    assert!(
        first_proposed_action(&events).is_none(),
        "a network-denied write must never park for approval"
    );
    assert!(created.is_empty());
    assert!(has_failed_tool(&events));
}

// --- Phase 3 STEP 3.4: source provenance on the read path ------------------

/// A driver that runs a fixed script and records the transcript it is handed on
/// each step, so a test can inspect what the model actually saw (e.g. a read
/// result's source-provenance label).
struct CapturingDriver {
    steps: std::sync::Mutex<std::collections::VecDeque<ModelStep>>,
    seen: Arc<Mutex<Vec<codypendent_runtime::agent::TurnItem>>>,
}

#[async_trait]
impl codypendent_runtime::agent::ModelDriver for CapturingDriver {
    fn model_id(&self) -> codypendent_protocol::ModelId {
        codypendent_protocol::ModelId("test".to_string())
    }
    async fn next_step(
        &self,
        transcript: &[codypendent_runtime::agent::TurnItem],
        sink: &mut dyn codypendent_runtime::agent::DeltaSink,
    ) -> anyhow::Result<StepOutcome> {
        *self.seen.lock().unwrap() = transcript.to_vec();
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ModelStep::Finish {
                summary: "done".to_string(),
            });
        if let ModelStep::Say(text) = &step {
            sink.on_text(text);
        }
        Ok(StepOutcome::unmeasured(step))
    }
}

/// Drive one `workspace.read_file` of `file` with `dirty_buffers` seeded on the
/// run, and return every transcript item the model was shown.
async fn read_with_ide_context(
    file: &std::path::Path,
    repo: &std::path::Path,
    dirty_buffers: Vec<codypendent_protocol::ide::DirtyBufferDigest>,
) -> Vec<codypendent_runtime::agent::TurnItem> {
    let dir = repo; // repo is a real dir owned by the caller
    let pool = open_database(&dir.join("db.sqlite")).await.unwrap();
    let store = ArtifactStore::new(dir.join("artifacts"));
    let broker = ApprovalBroker::new();
    let hub = SubscriptionHub::new();
    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(&pool, session, "ide-it")
        .await
        .unwrap();
    seed_started_run!(pool, session, run, "read", AgentMode::Build);

    let runtime = build_runtime!(pool, store, broker, hub);
    let mut rx = hub.subscribe(session);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let driver = CapturingDriver {
        steps: std::sync::Mutex::new(
            vec![ModelStep::CallTool {
                tool: "workspace.read_file".to_string(),
                args: json!({ "path": file.to_string_lossy() }),
            }]
            .into(),
        ),
        seen: seen.clone(),
    };

    let ctx = RunContext::new(session, run, "read", AgentMode::Build, repo, repo)
        .with_ide_context(dirty_buffers);
    let handle = tokio::spawn(async move {
        runtime
            .execute_run(&driver, ctx, CancellationToken::never())
            .await
    });

    loop {
        let event = rx.recv().await.expect("event");
        if matches!(event.body, EventBody::RunCompleted { .. }) {
            break;
        }
    }
    handle.await.unwrap().unwrap();
    let seen = seen.lock().unwrap().clone();
    seen
}

fn read_result_text(items: &[codypendent_runtime::agent::TurnItem]) -> String {
    items
        .iter()
        .filter_map(|item| match item {
            codypendent_runtime::agent::TurnItem::ToolResult { tool, output }
                if tool == "workspace.read_file" =>
            {
                Some(output.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn read_of_a_diverging_dirty_buffer_is_labeled_unsaved() {
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    let file = repo.join("src.rs");
    std::fs::write(&file, b"the committed, on-disk contents\n").unwrap();

    // A dirty buffer whose digest does NOT match the file on disk.
    let dirty = codypendent_protocol::ide::DirtyBufferDigest {
        path: file.to_string_lossy().into_owned(),
        sha256: "not-the-disk-digest".to_string(),
        byte_length: 3,
    };
    let seen = read_with_ide_context(&file, &repo, vec![dirty]).await;
    let text = read_result_text(&seen);
    assert!(
        text.contains("[source: unsaved-ide-buffer]"),
        "diverging dirty buffer must be labeled unsaved-ide-buffer; got: {text}"
    );
}

#[tokio::test]
async fn read_without_dirty_buffer_is_unlabeled() {
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    let file = repo.join("src.rs");
    std::fs::write(&file, b"plain filesystem read\n").unwrap();

    let seen = read_with_ide_context(&file, &repo, Vec::new()).await;
    let text = read_result_text(&seen);
    assert!(
        !text.contains("[source:"),
        "a plain filesystem read carries no source label; got: {text}"
    );
    assert!(text.contains("plain filesystem read"));
}

// ---------------------------------------------------------------------------
// Read-your-writes on an isolated worktree (T5 fix pass).
//
// An isolated run's read/search root and write root must be the SAME tree (the
// worktree), so a file the agent writes reads and searches back. The two probes
// below drive the real apply_patch → read_file → search path: with the fixed
// wiring (read root == worktree) both succeed and return the edit; with the
// pre-fix split (read root == repository, write == a sibling worktree) the read
// is denied and the search never sees the write.
// ---------------------------------------------------------------------------

/// The concatenated `ToolResult` outputs for `tool_name` in a captured transcript.
fn tool_result_text(items: &[codypendent_runtime::agent::TurnItem], tool_name: &str) -> String {
    items
        .iter()
        .filter_map(|item| match item {
            codypendent_runtime::agent::TurnItem::ToolResult { tool, output }
                if tool == tool_name =>
            {
                Some(output.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A sentinel the agent writes into the worktree, then reads and searches back.
const RYW_SENTINEL: &str = "READYOURWRITES_XYZZY";

/// Drive an agent (Build) that applies a patch to the worktree, then reads the
/// patched file back (relative path) and searches for its new content. Returns the
/// captured `(read_result, search_result)` observations. `read_root_is_worktree`
/// selects the fixed wiring (read root == the worktree) vs the pre-fix split (read
/// root == the repository), with the write root always the isolated worktree.
async fn read_your_writes_probe(read_root_is_worktree: bool) -> (String, String) {
    let base = tempfile::tempdir().unwrap();
    let repo = base.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    repo_with_committed_file(&repo, "src.txt", "hello\n").await;
    let repo = std::fs::canonicalize(&repo).unwrap();
    // A real isolated worktree (a checkout at HEAD OUTSIDE the repository) — the
    // shape WorktreeManager produces.
    let worktree = base.path().join("wt");
    assert!(
        git(
            &repo,
            &["worktree", "add", "--detach", worktree.to_str().unwrap()]
        )
        .await
        .status
        .success(),
        "git worktree add"
    );
    let worktree = std::fs::canonicalize(&worktree).unwrap();

    let pool = open_database(&base.path().join("db.sqlite")).await.unwrap();
    let store = ArtifactStore::new(base.path().join("artifacts"));
    let broker = ApprovalBroker::new();
    let hub = SubscriptionHub::new();
    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(&pool, session, "ryw").await.unwrap();
    seed_started_run!(pool, session, run, "ryw", AgentMode::Build);
    let runtime = build_runtime!(pool, store, broker, hub);

    // A patch that turns `src.txt`'s committed "hello" into the sentinel.
    let patch = format!(
        "diff --git a/src.txt b/src.txt\n\
         index 0000000..1111111 100644\n\
         --- a/src.txt\n\
         +++ b/src.txt\n\
         @@ -1 +1 @@\n\
         -hello\n\
         +{RYW_SENTINEL}\n"
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let driver = CapturingDriver {
        steps: std::sync::Mutex::new(
            vec![
                ModelStep::CallTool {
                    tool: "git.apply_patch".to_string(),
                    args: json!({ "patch": patch }),
                },
                ModelStep::CallTool {
                    tool: "workspace.read_file".to_string(),
                    // A RELATIVE path — resolved against the run's worktree.
                    args: json!({ "path": "src.txt" }),
                },
                ModelStep::CallTool {
                    tool: "workspace.search".to_string(),
                    args: json!({ "pattern": RYW_SENTINEL }),
                },
                ModelStep::Finish {
                    summary: "done".to_string(),
                },
            ]
            .into(),
        ),
        seen: seen.clone(),
    };

    let read_root = if read_root_is_worktree {
        worktree.clone()
    } else {
        repo.clone()
    };
    let ctx = RunContext::new(session, run, "ryw", AgentMode::Build, read_root, worktree);

    let mut rx = hub.subscribe(session);
    let handle = tokio::spawn(async move {
        runtime
            .execute_run(&driver, ctx, CancellationToken::never())
            .await
    });
    // Pump events, auto-approving the apply_patch write so the loop never blocks.
    loop {
        let event = rx.recv().await.expect("event");
        let done = matches!(event.body, EventBody::RunCompleted { .. });
        if let EventBody::ToolProposed { approval_id, .. } = &event.body {
            broker
                .resolve(
                    &pool,
                    *approval_id,
                    ApprovalDecision::Approve,
                    ApprovalScope::Once,
                    "tester".to_string(),
                )
                .await
                .unwrap();
        }
        if done {
            break;
        }
    }
    handle.await.unwrap().unwrap();

    let items = seen.lock().unwrap().clone();
    (
        tool_result_text(&items, "workspace.read_file"),
        tool_result_text(&items, "workspace.search"),
    )
}

#[tokio::test]
async fn isolated_worktree_agent_reads_and_searches_back_its_own_writes() {
    // The fix: read root == write root == the worktree. The agent's own write is
    // read back verbatim and found by search.
    let (read, search) = read_your_writes_probe(true).await;
    assert!(
        read.contains(RYW_SENTINEL),
        "read must return the agent's own edit, not stale/denied; got:\n{read}"
    );
    assert!(
        search.contains(RYW_SENTINEL),
        "search must find the agent's own edit in the worktree; got:\n{search}"
    );
}

#[tokio::test]
async fn split_read_root_cannot_read_or_search_back_worktree_writes() {
    // The pre-fix split (read root == repository, write == a sibling worktree): the
    // write lands in the worktree, but the read-back is DENIED (outside the read
    // root) and the search over the repository never sees it. This is exactly the
    // defect the executor fix closes by making read root == write root == worktree.
    let (read, search) = read_your_writes_probe(false).await;
    assert!(
        !read.contains(RYW_SENTINEL),
        "the split read root must NOT read the worktree write back; got:\n{read}"
    );
    assert!(
        read.contains("outside the allowed roots"),
        "the read-back is policy-denied out-of-scope (policy.path-out-of-scope); got:\n{read}"
    );
    assert!(
        !search.contains(RYW_SENTINEL),
        "search over the repository never sees the worktree write; got:\n{search}"
    );
}

#[tokio::test]
async fn explore_run_reads_the_repository_root() {
    // A read-only run keeps read root == write root == the repository root (no
    // isolated worktree). Reading a committed file there works, unchanged — the fix
    // must not regress the non-isolated path.
    let base = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(base.path()).unwrap();
    repo_with_committed_file(&repo, "src.txt", "explore-me\n").await;

    let pool = open_database(&base.path().join("db.sqlite")).await.unwrap();
    let store = ArtifactStore::new(base.path().join("artifacts"));
    let broker = ApprovalBroker::new();
    let hub = SubscriptionHub::new();
    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(&pool, session, "explore")
        .await
        .unwrap();
    seed_started_run!(pool, session, run, "look", AgentMode::Explore);
    let runtime = build_runtime!(pool, store, broker, hub);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let driver = CapturingDriver {
        steps: std::sync::Mutex::new(
            vec![
                ModelStep::CallTool {
                    tool: "workspace.read_file".to_string(),
                    args: json!({ "path": "src.txt" }),
                },
                ModelStep::Finish {
                    summary: "looked".to_string(),
                },
            ]
            .into(),
        ),
        seen: seen.clone(),
    };
    // Read-only: read root == write root == repo. No approval needed for a read.
    let ctx = RunContext::new(session, run, "look", AgentMode::Explore, repo.clone(), repo);
    let mut rx = hub.subscribe(session);
    let handle = tokio::spawn(async move {
        runtime
            .execute_run(&driver, ctx, CancellationToken::never())
            .await
    });
    loop {
        let event = rx.recv().await.expect("event");
        if matches!(event.body, EventBody::RunCompleted { .. }) {
            break;
        }
    }
    handle.await.unwrap().unwrap();

    let read = tool_result_text(&seen.lock().unwrap(), "workspace.read_file");
    assert!(
        read.contains("explore-me"),
        "an Explore run reads the repository root; got:\n{read}"
    );
}

#[tokio::test]
async fn a_single_agent_run_drives_github_writes_through_approval() {
    // The single-agent baseline (plain `StartRun`) can still drive the GitHub
    // repair sequence directly: read the failing check, run a test, then update the
    // PR and post a check summary — the two writes each parking for approval. This
    // is the Phase-1 loop capability that the declarative `repair-github-check`
    // workflow now packages as the `/fix-ci` product path (its `publish` step
    // covers the PR update; the check-run summary is the intentional divergence
    // documented in the workflow, still reachable from a plain run as shown here).
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    let pool = open_database(&dir.path().join("db.sqlite")).await.unwrap();
    let store = ArtifactStore::new(dir.path().join("artifacts"));
    let broker = ApprovalBroker::new();
    let hub = SubscriptionHub::new();
    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(&pool, session, "fixci")
        .await
        .unwrap();
    seed_started_run!(pool, session, run, "fix ci", AgentMode::Build);

    let gh = Arc::new(RecordingGitHub::default());
    let updated = gh.updated.clone();
    let summaries = gh.summaries.clone();

    let runtime = {
        let journal = run_journal!(pool, broker);
        let sink: Box<dyn ArtifactSink> = Box::new(store_sink!(store, pool));
        FrameworkAgentRuntime::new(
            ModelRegistry::new(Vec::new()),
            PolicyEngine::with_defaults_allowing_network([GITHUB_API_ENDPOINT.to_string()]),
            broker.clone(),
            hub.clone(),
            journal,
            sink,
        )
        .with_github(gh as Arc<dyn GitHubApi>)
    };

    let mut rx = hub.subscribe(session);
    let driver = ScriptedDriver::new(vec![
        ModelStep::CallTool {
            tool: "github.list_check_runs".to_string(),
            args: json!({ "ref": "main" }),
        },
        ModelStep::CallTool {
            tool: "shell.run".to_string(),
            args: json!({ "program": "git", "args": ["--version"] }),
        },
        ModelStep::CallTool {
            tool: "github.update_pull_request".to_string(),
            args: json!({ "number": 7, "body": "fixed the failing check" }),
        },
        ModelStep::CallTool {
            tool: "github.create_check_run_summary".to_string(),
            args: json!({ "name": "ci", "head_sha": "abc", "summary": "green", "conclusion": "success" }),
        },
        ModelStep::Finish {
            summary: "fixed".to_string(),
        },
    ]);
    let ctx = RunContext::new(session, run, "fix ci", AgentMode::Build, repo.clone(), repo)
        .with_github_repo(RepoId::new("octocat", "hello-world"));
    let handle = tokio::spawn(async move {
        runtime
            .execute_run(&driver, ctx, CancellationToken::never())
            .await
    });

    let mut github_mutations = 0;
    loop {
        let event = rx.recv().await.expect("event");
        let done = matches!(event.body, EventBody::RunCompleted { .. });
        if let EventBody::ToolProposed {
            approval_id,
            action,
            ..
        } = &event.body
        {
            if matches!(action, ProposedAction::GitHubMutation { .. }) {
                github_mutations += 1;
            }
            broker
                .resolve(
                    &pool,
                    *approval_id,
                    ApprovalDecision::Approve,
                    ApprovalScope::Once,
                    "op".to_string(),
                )
                .await
                .unwrap();
        }
        if done {
            break;
        }
    }
    handle.await.unwrap().unwrap();

    // Both writes happened, and each was an approval-gated GitHubMutation.
    assert_eq!(*updated.lock().unwrap(), vec![7]);
    assert_eq!(summaries.lock().unwrap().len(), 1);
    assert_eq!(
        github_mutations, 2,
        "the PR update and the check summary each parked for approval"
    );
}

/// A no-op blackboard channel: enough to make the tools *available* (the runtime
/// only checks `is_some()` to decide whether to offer them) without a store.
struct FakeBlackboardChannel;

#[async_trait]
impl BlackboardChannel for FakeBlackboardChannel {
    async fn post(
        &self,
        _workflow_run_id: &str,
        _post: BlackboardPost,
    ) -> Result<BlackboardItemView, BlackboardChannelError> {
        Err(BlackboardChannelError::Backend("fake channel".to_string()))
    }
    async fn query(
        &self,
        _workflow_run_id: &str,
        _kind: Option<String>,
        _include_superseded: bool,
    ) -> Result<Vec<BlackboardItemView>, BlackboardChannelError> {
        Ok(Vec::new())
    }
}

/// STEP 5.3 test 4: the `blackboard.*` tools are offered ONLY to a workflow agent
/// node (a `RunContext` carrying a `WorkflowContext`), never to a plain
/// single-agent run — even when a channel is wired. Asserts both the offered-tool
/// set and the dispatch behaviour: a single-agent run that calls `blackboard.post`
/// gets an unknown-tool refusal (the tool is not offered), keeping that baseline
/// clean.
#[tokio::test]
async fn blackboard_tools_are_offered_only_inside_a_workflow_run() {
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    repo_with_committed_file(&repo, "a.txt", "hello\n").await;
    let pool = open_database(&dir.path().join("db.sqlite")).await.unwrap();
    let store = ArtifactStore::new(dir.path().join("artifacts"));
    let broker = ApprovalBroker::new();
    let hub = SubscriptionHub::new();
    let runtime =
        build_runtime!(pool, store, broker, hub).with_blackboard(Arc::new(FakeBlackboardChannel));

    // The registered-tool set: a single-agent run is NOT offered the blackboard
    // tools; a workflow node IS.
    let single = RunContext::new(
        SessionId::new(),
        RunId::new(),
        "solo",
        AgentMode::Build,
        repo.clone(),
        repo.clone(),
    );
    let node = RunContext::new(
        SessionId::new(),
        RunId::new(),
        "node",
        AgentMode::Build,
        repo.clone(),
        repo.clone(),
    )
    .with_workflow(WorkflowContext {
        workflow_run_id: "wfrun-1".to_string(),
        node_id: "inspect".to_string(),
        agent_role: "investigator".to_string(),
    });

    let solo_tools = runtime.offered_tool_names(&single);
    assert!(
        !solo_tools.contains(&BlackboardPostTool::NAME)
            && !solo_tools.contains(&BlackboardQueryTool::NAME),
        "a single-agent run is not offered the blackboard tools: {solo_tools:?}"
    );
    let node_tools = runtime.offered_tool_names(&node);
    assert!(
        node_tools.contains(&BlackboardPostTool::NAME)
            && node_tools.contains(&BlackboardQueryTool::NAME),
        "a workflow node is offered the blackboard tools: {node_tools:?}"
    );

    // Dispatch behaviour: a single-agent run that calls blackboard.post is refused
    // as an unknown tool (not offered) — the baseline never touches the board.
    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(&pool, session, "solo")
        .await
        .unwrap();
    seed_started_run!(pool, session, run, "solo", AgentMode::Build);
    let mut rx = hub.subscribe(session);
    let driver = ScriptedDriver::new(vec![
        ModelStep::CallTool {
            tool: BlackboardPostTool::NAME.to_string(),
            args: json!({ "kind": "finding", "payload": {}, "evidence": [{}] }),
        },
        ModelStep::Finish {
            summary: "done".to_string(),
        },
    ]);
    let ctx = RunContext::new(
        session,
        run,
        "solo",
        AgentMode::Build,
        repo.clone(),
        repo.clone(),
    );
    let handle = tokio::spawn(async move {
        runtime
            .execute_run(&driver, ctx, CancellationToken::never())
            .await
    });

    let mut post_failed_unknown = false;
    loop {
        let event = rx.recv().await.expect("event");
        if let EventBody::ToolCompleted { tool, outcome, .. } = &event.body {
            if tool == BlackboardPostTool::NAME {
                match outcome {
                    ToolOutcome::Failed { message } => {
                        assert!(
                            message.contains("unknown tool"),
                            "blackboard.post in a single-agent run is an unknown tool: {message}"
                        );
                        post_failed_unknown = true;
                    }
                    other => panic!("expected a Failed outcome, got {other:?}"),
                }
            }
        }
        if matches!(event.body, EventBody::RunCompleted { .. }) {
            break;
        }
    }
    handle.await.unwrap().unwrap();
    assert!(
        post_failed_unknown,
        "the single-agent blackboard.post call must be refused as unknown"
    );
}
