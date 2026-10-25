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
use codypendent_integrations::mcp::{McpBridge, McpError, McpToolInfo};
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
        let state_pool = $pool.clone();
        let terminal_pool = $pool.clone();
        let approve_broker = $broker.clone();
        RunJournal::new(
            move |session: SessionId, actor: Actor, body: EventBody| {
                let pool = persist_pool.clone();
                async move {
                    // Use the daemon's atomic transition guard: a permissive
                    // projection write can hide duplicate or stale transitions.
                    match &body {
                        EventBody::RunStateChanged { run_id, state } => {
                            return ledger::append_run_state_changed(
                                &pool,
                                session,
                                &actor,
                                *run_id,
                                *state,
                                Utc::now(),
                            )
                            .await;
                        }
                        body @ EventBody::RunUsage { .. } => {
                            return ledger::append_run_usage(
                                &pool,
                                session,
                                &actor,
                                body,
                                Utc::now(),
                            )
                            .await;
                        }
                        _ => {}
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
                            req.repository.as_deref(),
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
        .with_terminal_persist(move |session, actor, state, body| {
            let pool = terminal_pool.clone();
            async move {
                ledger::append_run_terminal(&pool, session, &actor, state, &body, Utc::now()).await
            }
        })
        .with_state_reader(move |run_id| {
            let pool = state_pool.clone();
            async move { projections::load_run_state(&pool, run_id).await }
        })
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
        prompt_tokens: Some(10),
        completion_tokens: Some(5),
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
            prompt_tokens: Some(20),
            completion_tokens: Some(10),
            cost_micros: Some(2_000),
        }),
        "two measured requests sum into the run's aggregated usage"
    );
    let events = ledger::load_events(&pool, session).await.unwrap();
    let usage_events = events
        .iter()
        .filter(|event| matches!(event.body, EventBody::RunUsage { .. }))
        .collect::<Vec<_>>();
    assert_eq!(usage_events.len(), 1, "usage is emitted exactly once");
    let usage_sequence = usage_events[0].sequence;
    let terminal_sequence = events
        .iter()
        .find(|event| {
            matches!(
                event.body,
                EventBody::RunStateChanged {
                    state: RunState::Completed,
                    ..
                }
            )
        })
        .expect("terminal state event")
        .sequence;
    let completion_sequence = events
        .iter()
        .find(|event| matches!(event.body, EventBody::RunCompleted { .. }))
        .expect("completion event")
        .sequence;
    assert!(
        usage_sequence < terminal_sequence && terminal_sequence < completion_sequence,
        "usage and terminal state must both precede the close-enabling completion barrier"
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
    assert!(
        !ledger::load_events(&pool, session2)
            .await
            .unwrap()
            .iter()
            .any(|event| matches!(event.body, EventBody::RunUsage { .. })),
        "an unmeasured run emits no all-None usage event"
    );
    let unmeasured: (Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT prompt_tokens, completion_tokens, cost_micros FROM runs WHERE id = ?",
    )
    .bind(run2.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unmeasured, (None, None, None));
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
    // FIX 3 (agent & tool fixes spec) is SCOPED to the `shell.run`
    // not-allow-listed denial (`policy.program-not-allowlisted`): a write refused
    // by mode is a different reason code and must not carry the shell hint.
    assert!(
        !message.contains("workspace.read_file"),
        "a write-denied-by-mode message must not carry the shell allow-list hint: {message}"
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

#[tokio::test]
async fn approving_a_paused_run_waits_for_resume_or_cancellation() {
    for resume in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(dir.path()).unwrap();
        repo_with_committed_file(&repo, "a.txt", "hello\n").await;
        let pool = open_database(&dir.path().join("db.sqlite")).await.unwrap();
        let store = ArtifactStore::new(dir.path().join("artifacts"));
        let broker = ApprovalBroker::new();
        let hub = SubscriptionHub::new();
        let session = SessionId::new();
        let run = RunId::new();
        ledger::create_session(&pool, session, "paused approval")
            .await
            .unwrap();
        projections::insert_run(
            &pool,
            run,
            session,
            "inspect",
            AgentMode::Build,
            "hosted",
            "{}",
        )
        .await
        .unwrap();
        let runtime = build_runtime!(pool, store, broker, hub);
        let mut rx = hub.subscribe(session);
        let driver = ScriptedDriver::new(vec![
            ModelStep::CallTool {
                tool: "shell.run".to_string(),
                args: json!({"program": "git", "args": ["--version"]}),
            },
            ModelStep::Finish {
                summary: "done".to_string(),
            },
        ]);
        let ctx = RunContext::new(session, run, "inspect", AgentMode::Build, &repo, &repo);
        let (handle, token) = cancellation();
        let task = tokio::spawn(async move { runtime.execute_run(&driver, ctx, token).await });
        let approval_id = loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("approval proposal arrives")
                .unwrap();
            if let EventBody::ToolProposed { approval_id, .. } = event.body {
                break approval_id;
            }
        };
        // Mirror PauseRun's durable state and runtime control before resolving.
        projections::set_run_state(&pool, run, RunState::Paused)
            .await
            .unwrap();
        handle.pause();
        broker
            .resolve(
                &pool,
                approval_id,
                ApprovalDecision::Approve,
                ApprovalScope::Once,
                "tester".into(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
                .await
                .is_err(),
            "approval must neither publish Running nor start a tool while paused"
        );
        assert_eq!(
            projections::load_run_state(&pool, run).await.unwrap(),
            Some(RunState::Paused)
        );
        assert!(!task.is_finished());
        if resume {
            // ResumeRun persists Running before waking the runtime. Its
            // approval continuation must not emit another Running transition.
            projections::set_run_state(&pool, run, RunState::Running)
                .await
                .unwrap();
            handle.resume();
        } else {
            handle.cancel();
        }
        let disposition = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("run settles after explicit control")
            .unwrap()
            .unwrap()
            .disposition;
        assert_eq!(
            matches!(disposition, RunDisposition::Completed { .. }),
            resume
        );
        assert_eq!(
            matches!(disposition, RunDisposition::Cancelled { .. }),
            !resume
        );
        let events = ledger::load_events(&pool, session).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.body, EventBody::ToolStarted { .. }))
                .count(),
            usize::from(resume),
            "an approved command executes exactly once after resume, or never after cancel"
        );
    }
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
        // A never-merged open PR: the fixture's `state` is "open", so the merge
        // fields must agree with it rather than defaulting arbitrarily.
        merged: false,
        merged_at: None,
        merge_commit_sha: None,
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
        _tools: &[codypendent_runtime::agent::ToolDefinition],
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
            codypendent_runtime::agent::TurnItem::ToolResult { tool, output, .. }
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

// --- loop-fix Task 1: transcript-fidelity FIXes 1 & 2 -----------------------

#[tokio::test]
async fn read_file_tool_call_is_recorded_immediately_before_its_result() {
    // FIX 1 (transcript fidelity): before this fix, only the `ToolResult` was
    // ever recorded — a replayed transcript had no memory of what the model
    // had asked for, only what came back. The model's request must now
    // appear as its own transcript item, paired immediately before its
    // result (asked -> result), so a replay can tell "I already asked for
    // this" instead of re-issuing the same call.
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    let file = repo.join("src.rs");
    std::fs::write(&file, b"hello\n").unwrap();

    let seen = read_with_ide_context(&file, &repo, Vec::new()).await;
    use codypendent_runtime::agent::TurnItem;
    let call_index = seen
        .iter()
        .position(
            |item| matches!(item, TurnItem::ToolCall { tool, .. } if tool == "workspace.read_file"),
        )
        .expect("a ToolCall item must be recorded for the model's request");
    match seen.get(call_index + 1) {
        Some(TurnItem::ToolResult { tool, .. }) if tool == "workspace.read_file" => {}
        other => panic!(
            "expected the ToolCall to be immediately followed by its ToolResult, got {other:?}"
        ),
    }
}

#[tokio::test]
async fn read_file_observation_carries_a_path_header() {
    // FIX 2 (transcript fidelity): the read_file observation must be tied to
    // the file it read, so a REPLAYED `[tool result: workspace.read_file]`
    // isn't anonymous line-numbered text with no filename attached.
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    let file = repo.join("annotated.rs");
    std::fs::write(&file, b"fn main() {}\n").unwrap();

    let seen = read_with_ide_context(&file, &repo, Vec::new()).await;
    let text = read_result_text(&seen);
    let expected_path = file.display().to_string();
    assert!(
        text.contains(&expected_path),
        "the read_file observation must name the file it read; got:\n{text}"
    );
    assert!(
        text.contains("lines 1-"),
        "the read_file observation must carry the line range; got:\n{text}"
    );
}

// --- loop-fix Task 2: repeated-identical-tool-call guard --------------------
//
// Task 1 (above) fixed the ROOT CAUSE of a re-reading loop by giving the
// model a transcript that records its own `ToolCall` turns, so a replay lets
// it notice "I already asked for this". This guard is the DEFENSE-IN-DEPTH
// backstop for a model that still ignores that and re-issues the identical
// call: past a threshold of CONSECUTIVE identical `(tool, args)` calls, the
// loop stops executing the tool and steers the model instead. The threshold
// mirrors `MAX_CONSECUTIVE_IDENTICAL_CALLS` in `crates/runtime/src/agent.rs`
// (currently 3) — kept in sync manually since the const is private to that
// module.

/// Drive `steps` (typically some `CallTool`s followed by a `Finish`) through
/// a fresh Build-mode run in a temp repo seeded with `files`
/// (relative-path -> contents), auto-approving any `ToolProposed` so the loop
/// never blocks. Returns every transcript item the model was shown — the
/// `CapturingDriver` overwrites `seen` on each `next_step` call, and its last
/// call (right before `Finish`) sees the whole conversation so far.
async fn drive_steps(
    steps: Vec<ModelStep>,
    files: &[(&str, &str)],
) -> Vec<codypendent_runtime::agent::TurnItem> {
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    for (name, contents) in files {
        std::fs::write(repo.join(name), contents).unwrap();
    }

    let pool = open_database(&dir.path().join("db.sqlite")).await.unwrap();
    let store = ArtifactStore::new(dir.path().join("artifacts"));
    let broker = ApprovalBroker::new();
    let hub = SubscriptionHub::new();
    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(&pool, session, "guard-it")
        .await
        .unwrap();
    seed_started_run!(pool, session, run, "guard", AgentMode::Build);

    let runtime = build_runtime!(pool, store, broker, hub);
    let mut rx = hub.subscribe(session);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let driver = CapturingDriver {
        steps: std::sync::Mutex::new(steps.into()),
        seen: seen.clone(),
    };

    let ctx = RunContext::new(session, run, "guard", AgentMode::Build, repo.clone(), repo);
    let handle = tokio::spawn(async move {
        runtime
            .execute_run(&driver, ctx, CancellationToken::never())
            .await
    });

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

    let seen = seen.lock().unwrap().clone();
    seen
}

#[tokio::test]
async fn repeated_identical_tool_call_short_circuits_after_threshold() {
    // The model issues the SAME `workspace.read_file` call five times in a
    // row (identical tool + args, back to back — the exact shape of the real
    // failing run this guard was written for). The first two run for real;
    // the third, fourth, and fifth are short-circuited: the tool's
    // underlying execution runs at most (threshold - 1) == 2 times, and each
    // short-circuited step gets a DISTINCT "already called" steer instead of
    // a repeated/fabricated tool result.
    use codypendent_runtime::agent::TurnItem;

    let call = ModelStep::CallTool {
        tool: "workspace.read_file".to_string(),
        args: json!({ "path": "target.txt" }),
    };
    let steps = vec![
        call.clone(),
        call.clone(),
        call.clone(),
        call.clone(),
        call,
        ModelStep::Finish {
            summary: "done".to_string(),
        },
    ];
    let seen = drive_steps(steps, &[("target.txt", "hello\n")]).await;

    let call_count = seen
        .iter()
        .filter(
            |item| matches!(item, TurnItem::ToolCall { tool, .. } if tool == "workspace.read_file"),
        )
        .count();
    assert_eq!(
        call_count, 5,
        "the transcript must honestly record every call the model asked for, \
         even the short-circuited ones; got:\n{seen:?}"
    );

    let result_count = seen
        .iter()
        .filter(|item| matches!(item, TurnItem::ToolResult { tool, .. } if tool == "workspace.read_file"))
        .count();
    assert_eq!(
        result_count, 2,
        "the tool must actually execute at most (threshold - 1) == 2 times; \
         the rest must be short-circuited; got:\n{seen:?}"
    );

    let steer_count = seen
        .iter()
        .filter(|item| matches!(item, TurnItem::Steering(text) if text.contains("already called")))
        .count();
    assert_eq!(
        steer_count, 3,
        "each short-circuited call must inject a distinct 'already called' \
         steer, not a repeated tool result; got:\n{seen:?}"
    );
    let steer_text = seen
        .iter()
        .find_map(|item| match item {
            TurnItem::Steering(text) if text.contains("already called") => Some(text.clone()),
            _ => None,
        })
        .expect("at least one 'already called' steer");
    assert!(
        steer_text.contains("workspace.read_file"),
        "the steer must name the repeated tool; got: {steer_text}"
    );
    assert!(
        steer_text.contains("Do not repeat this call"),
        "the steer must tell the model to stop repeating and use the prior \
         result; got: {steer_text}"
    );
}

#[tokio::test]
async fn different_call_in_between_resets_the_consecutive_counter() {
    // Control: two identical calls, then a DIFFERENT call, then the first
    // identity twice more. No identity ever reaches the threshold
    // consecutively (the intervening different call resets the streak each
    // time), so every call must execute for real — the guard must never
    // fire on legitimately-spaced repeats.
    use codypendent_runtime::agent::TurnItem;

    let call_a = ModelStep::CallTool {
        tool: "workspace.read_file".to_string(),
        args: json!({ "path": "a.txt" }),
    };
    let call_b = ModelStep::CallTool {
        tool: "workspace.read_file".to_string(),
        args: json!({ "path": "b.txt" }),
    };
    let steps = vec![
        call_a.clone(),
        call_a.clone(),
        call_b,
        call_a.clone(),
        call_a,
        ModelStep::Finish {
            summary: "done".to_string(),
        },
    ];
    let seen = drive_steps(steps, &[("a.txt", "A\n"), ("b.txt", "B\n")]).await;

    let a_results = seen
        .iter()
        .filter(|item| {
            matches!(item, TurnItem::ToolResult { tool, output, .. } if tool == "workspace.read_file" && output.contains("a.txt"))
        })
        .count();
    let b_results = seen
        .iter()
        .filter(|item| {
            matches!(item, TurnItem::ToolResult { tool, output, .. } if tool == "workspace.read_file" && output.contains("b.txt"))
        })
        .count();
    assert_eq!(
        a_results, 4,
        "all four `a.txt` reads must execute for real (no premature \
         short-circuit); got:\n{seen:?}"
    );
    assert_eq!(
        b_results, 1,
        "the intervening `b.txt` read must execute for real; got:\n{seen:?}"
    );

    let premature_steer = seen
        .iter()
        .any(|item| matches!(item, TurnItem::Steering(text) if text.contains("already called")));
    assert!(
        !premature_steer,
        "a different call in between must reset the consecutive counter — \
         the guard must not fire; got:\n{seen:?}"
    );
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
            codypendent_runtime::agent::TurnItem::ToolResult { tool, output, .. }
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
        !solo_tools.iter().any(|n| n == BlackboardPostTool::NAME)
            && !solo_tools.iter().any(|n| n == BlackboardQueryTool::NAME),
        "a single-agent run is not offered the blackboard tools: {solo_tools:?}"
    );
    let node_tools = runtime.offered_tool_names(&node);
    assert!(
        node_tools.iter().any(|n| n == BlackboardPostTool::NAME)
            && node_tools.iter().any(|n| n == BlackboardQueryTool::NAME),
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

// --- PR B: MCP client tools through the gated loop -------------------------

/// An in-memory stub bridge (no processes): a fixed offered-tool cache and a
/// scripted result string. The registry's own duplex tests cover the wire.
struct StubMcpBridge {
    tools: Vec<McpToolInfo>,
    result: String,
}

impl StubMcpBridge {
    /// A warm bridge offering `mcp.fake.search`, returning `result` per call.
    fn warm(result: &str) -> Self {
        Self {
            tools: vec![McpToolInfo {
                server: "fake".to_string(),
                name: "search".to_string(),
                description: "search things".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"q": {"type": "string"}}
                }),
            }],
            result: result.to_string(),
        }
    }
}

#[async_trait]
impl McpBridge for StubMcpBridge {
    fn offered_tools(&self) -> Vec<McpToolInfo> {
        self.tools.clone()
    }

    async fn call_tool(
        &self,
        _server: &str,
        _tool: &str,
        _args: serde_json::Value,
    ) -> Result<String, McpError> {
        Ok(self.result.clone())
    }
}

/// Drive a run of `steps` against a warm stub MCP bridge under `policy`,
/// resolving any parked approval with `decision` at `scope` (the deny path
/// emits no `ToolProposed`, so resolution simply never fires there). Returns
/// the temp dir (kept alive so the DB survives), the published events, the
/// transcript the driver last observed (so a test can inspect the observation
/// the model was fed back), and how many approvals the test ACTUALLY resolved —
/// a Run-scoped auto-approval arrives already resolved, so it never counts.
async fn run_mcp(
    policy: PolicyEngine,
    bridge_result: &str,
    steps: Vec<ModelStep>,
    decision: ApprovalDecision,
    scope: ApprovalScope,
) -> (
    tempfile::TempDir,
    Vec<SessionEvent>,
    Vec<codypendent_runtime::agent::TurnItem>,
    usize,
) {
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    let pool = open_database(&dir.path().join("db.sqlite")).await.unwrap();
    let store = ArtifactStore::new(dir.path().join("artifacts"));
    let broker = ApprovalBroker::new();
    let hub = SubscriptionHub::new();

    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(&pool, session, "mcp-it")
        .await
        .unwrap();
    seed_started_run!(pool, session, run, "search", AgentMode::Build);

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
        .with_mcp(Arc::new(StubMcpBridge::warm(bridge_result)))
    };

    let mut rx = hub.subscribe(session);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let driver = CapturingDriver {
        steps: std::sync::Mutex::new(steps.into_iter().collect()),
        seen: seen.clone(),
    };
    let ctx = RunContext::new(session, run, "search", AgentMode::Build, repo.clone(), repo);

    let handle = tokio::spawn(async move {
        runtime
            .execute_run(&driver, ctx, CancellationToken::never())
            .await
    });

    let mut events = Vec::new();
    let mut resolved = 0usize;
    loop {
        let event = rx.recv().await.expect("event");
        let done = matches!(event.body, EventBody::RunCompleted { .. });
        if let EventBody::ToolProposed { approval_id, .. } = &event.body {
            match broker
                .resolve(&pool, *approval_id, decision, scope, "tester".to_string())
                .await
            {
                Ok(_) => resolved += 1,
                // Run-scoped auto-approval: the broker resolved the repeat at
                // `request` time (resolved_by `auto:run-scope`), so there is
                // nothing left for the approver to do — and no park happened.
                Err(codypendent_daemon::approvals::ApprovalError::AlreadyResolved { .. }) => {}
                Err(error) => panic!("approval resolution failed: {error}"),
            }
        }
        events.push(event);
        if done {
            break;
        }
    }
    handle.await.unwrap().unwrap();
    let transcript = seen.lock().unwrap().clone();
    (dir, events, transcript, resolved)
}

/// The observation a completed `tool` call fed back into the transcript.
fn mcp_observation(
    transcript: &[codypendent_runtime::agent::TurnItem],
    tool: &str,
) -> Option<String> {
    transcript.iter().find_map(|item| match item {
        codypendent_runtime::agent::TurnItem::ToolResult {
            tool: t, output, ..
        } if t == tool => Some(output.clone()),
        _ => None,
    })
}

/// PR B, the happy path: the default policy gates the MCP call
/// (RequireApproval), the run parks on an `McpToolCall` proposal, approval lets
/// it execute, and the result reaches the transcript ONLY as a sanitized,
/// origin-labeled evidence block — ANSI/control characters stripped, never raw.
#[tokio::test]
async fn mcp_tool_call_parks_for_approval_then_executes_sanitized() {
    let (_dir, events, transcript, resolved) = run_mcp(
        PolicyEngine::with_defaults(),
        "hits: \x1b[31mred\x1b[0m\x07 done",
        vec![
            ModelStep::CallTool {
                tool: "mcp.fake.search".to_string(),
                args: json!({"q": "x"}),
            },
            ModelStep::Finish {
                summary: "done".to_string(),
            },
        ],
        ApprovalDecision::Approve,
        ApprovalScope::Once,
    )
    .await;

    match first_proposed_action(&events) {
        Some(ProposedAction::McpToolCall {
            server,
            tool,
            summary,
            args,
        }) => {
            assert_eq!(server, "fake");
            assert_eq!(tool, "search");
            assert_eq!(args, "{\"q\":\"x\"}", "canonical args on the action");
            assert!(
                summary.contains("fake.search("),
                "the card summary names the call: {summary}"
            );
        }
        other => panic!("expected an McpToolCall proposal, got {other:?}"),
    }
    assert_eq!(resolved, 1, "the parked approval was resolved by the test");
    assert!(
        events.iter().any(|e| matches!(
            &e.body,
            EventBody::ToolCompleted {
                tool,
                outcome: ToolOutcome::Succeeded,
                ..
            } if tool == "mcp.fake.search"
        )),
        "the approved call executed"
    );

    let observation =
        mcp_observation(&transcript, "mcp.fake.search").expect("a tool result was fed back");
    assert!(
        observation.starts_with("[untrusted output from mcp:fake]\n"),
        "the sanitizer's evidence framing: {observation:?}"
    );
    assert!(
        observation.contains("hits: red done"),
        "content survives sanitization: {observation:?}"
    );
    assert!(
        !observation.contains('\x1b') && !observation.contains('\x07'),
        "ANSI/control characters are stripped: {observation:?}"
    );
}

/// PR B: with the first approval granted at Run scope, an identical second call
/// — args keys in a DIFFERENT order — canonicalizes to the same action digest
/// and auto-approves: the test resolves exactly ONE approval (the repeat is
/// broker-resolved as `auto:run-scope`, never parked on a human), and both
/// calls execute.
#[tokio::test]
async fn mcp_identical_repeat_auto_approves_under_run_scope_despite_key_order() {
    let (_dir, events, _transcript, resolved) = run_mcp(
        PolicyEngine::with_defaults(),
        "ok",
        vec![
            ModelStep::CallTool {
                tool: "mcp.fake.search".to_string(),
                args: json!({"a": 1, "q": "x"}),
            },
            ModelStep::CallTool {
                tool: "mcp.fake.search".to_string(),
                args: json!({"q": "x", "a": 1}),
            },
            ModelStep::Finish {
                summary: "done".to_string(),
            },
        ],
        ApprovalDecision::Approve,
        ApprovalScope::Run,
    )
    .await;

    assert_eq!(
        resolved, 1,
        "the key-reordered identical repeat digests the same and auto-approves — \
         only the FIRST call parked on a human resolution"
    );
    let completions = events
        .iter()
        .filter(|e| {
            matches!(
                &e.body,
                EventBody::ToolCompleted {
                    tool,
                    outcome: ToolOutcome::Succeeded,
                    ..
                } if tool == "mcp.fake.search"
            )
        })
        .count();
    assert_eq!(completions, 2, "both calls executed");
}

/// PR B: a policy-denied server never reaches the approval gate — the
/// observation is the legible `policy denied` refusal.
#[tokio::test]
async fn mcp_denied_server_never_parks_and_reports_policy_denied() {
    let policy_dir = tempfile::tempdir().unwrap();
    let policy_path = policy_dir.path().join("policy.toml");
    std::fs::write(&policy_path, "[mcp.servers]\nfake = \"deny\"\n").unwrap();
    let policy = PolicyEngine::load(Some(&policy_path), None).unwrap();

    let (_dir, events, transcript, _resolved) = run_mcp(
        policy,
        "unreachable",
        vec![
            ModelStep::CallTool {
                tool: "mcp.fake.search".to_string(),
                args: json!({"q": "x"}),
            },
            ModelStep::Finish {
                summary: "done".to_string(),
            },
        ],
        ApprovalDecision::Approve,
        ApprovalScope::Once,
    )
    .await;

    assert!(
        first_proposed_action(&events).is_none(),
        "a denied call must never park for approval"
    );
    let denial = events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ToolCompleted {
                tool,
                outcome: ToolOutcome::Failed { message },
                ..
            } if tool == "mcp.fake.search" => Some(message.clone()),
            _ => None,
        })
        .expect("the call completed as a policy denial");
    assert!(
        denial.starts_with("policy denied"),
        "the legible refusal: {denial}"
    );
    assert!(
        mcp_observation(&transcript, "mcp.fake.search")
            .is_some_and(|text| text.starts_with("policy denied")),
        "the denial is what the model is fed back"
    );
}

/// PR B: a call to a server.tool pair the bridge does NOT currently offer (a
/// cold server, or a tool `tools/list` never returned) is the SAME unknown-tool
/// refusal the offering gate implies — offered ≡ dispatchable.
#[tokio::test]
async fn mcp_call_the_bridge_does_not_offer_is_an_unknown_tool() {
    let (_dir, events, _transcript, _resolved) = run_mcp(
        PolicyEngine::with_defaults(),
        "unreachable",
        vec![
            ModelStep::CallTool {
                tool: "mcp.fake.nonexistent".to_string(),
                args: json!({}),
            },
            ModelStep::Finish {
                summary: "done".to_string(),
            },
        ],
        ApprovalDecision::Approve,
        ApprovalScope::Once,
    )
    .await;

    let failure = events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ToolCompleted {
                tool,
                outcome: ToolOutcome::Failed { message },
                ..
            } if tool == "mcp.fake.nonexistent" => Some(message.clone()),
            _ => None,
        })
        .expect("the call completed as a refusal");
    assert_eq!(failure, "unknown tool `mcp.fake.nonexistent`");
}

/// Coverage survives persistence and failures without discarding the measured
/// subtotal used by budgets. Each dimension is independently measured.
#[tokio::test]
async fn usage_coverage_preserves_partial_zero_retry_and_failed_requests() {
    use codypendent_runtime::agent::ToolDefinition;
    use codypendent_runtime::agent::{DeltaSink, ModelDriver, RetryNotice, TurnItem};
    struct Mixed {
        calls: Mutex<usize>,
        fail: bool,
    }
    #[async_trait]
    impl ModelDriver for Mixed {
        fn model_id(&self) -> codypendent_protocol::ModelId {
            codypendent_protocol::ModelId("mixed".into())
        }
        async fn next_step(
            &self,
            _: &[TurnItem],
            _: &[ToolDefinition],
            sink: &mut dyn DeltaSink,
        ) -> anyhow::Result<StepOutcome> {
            let first = {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                *calls == 1
            };
            if !first && self.fail {
                anyhow::bail!("provider disconnected");
            }
            if first {
                sink.on_retry(&RetryNotice {
                    attempt: 2,
                    max_attempts: 3,
                    message: "temporary failure".into(),
                    delay_ms: 0,
                });
            }
            Ok(StepOutcome {
                step: if first {
                    ModelStep::Say("working".into())
                } else {
                    ModelStep::Finish {
                        summary: "done".into(),
                    }
                },
                usage: Some(if first {
                    ModelUsage {
                        prompt_tokens: Some(0),
                        completion_tokens: Some(5),
                        cost_micros: None,
                    }
                } else {
                    ModelUsage {
                        prompt_tokens: None,
                        completion_tokens: Some(7),
                        cost_micros: Some(11),
                    }
                }),
                preface: None,
                extra_calls: vec![],
            })
        }
    }
    for fail in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap();
        let pool = open_database(&dir.path().join("usage.sqlite"))
            .await
            .unwrap();
        let store = ArtifactStore::new(dir.path().join("artifacts"));
        let broker = ApprovalBroker::new();
        let hub = SubscriptionHub::new();
        let runtime = build_runtime!(pool, store, broker, hub);
        let session = SessionId::new();
        let run = RunId::new();
        ledger::create_session(&pool, session, "mixed usage")
            .await
            .unwrap();
        seed_started_run!(pool, session, run, "read", AgentMode::Explore);
        let outcome = runtime
            .execute_run(
                &Mixed {
                    calls: Mutex::new(0),
                    fail,
                },
                RunContext::new(session, run, "read", AgentMode::Explore, repo.clone(), repo),
                CancellationToken::never(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.usage.unwrap().prompt_tokens, Some(0));
        let events = ledger::load_events(&pool, session).await.unwrap();
        let (coverage, cost) = events
            .iter()
            .find_map(|event| match event.body {
                EventBody::RunUsage {
                    coverage: Some(coverage),
                    cost_micros,
                    ..
                } => Some((coverage, cost_micros)),
                _ => None,
            })
            .expect("durable usage coverage");
        assert_eq!(coverage.requests, 2, "failed attempts count too");
        assert_eq!(coverage.prompt_requests, 1);
        assert_eq!(coverage.completion_requests, if fail { 1 } else { 2 });
        assert_eq!(coverage.cost_requests, u64::from(!fail));
        assert_eq!(coverage.unmeasured_retries, 1);
        assert_eq!(cost, if fail { None } else { Some(11) });
        let (stored,): (String,) = sqlx::query_as("SELECT usage_coverage FROM runs WHERE id = ?")
            .bind(run.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<codypendent_protocol::UsageCoverage>(&stored).unwrap(),
            coverage
        );
    }
}
