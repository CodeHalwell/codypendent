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
    Actor, AgentMode, ApprovalDecision, ApprovalScope, DataClassification, EventBody,
    ProposedAction, RunDisposition, RunId, RunState, SessionEvent, SessionId,
};
use codypendent_runtime::agent::{
    cancellation, ApprovalRequest, CancellationToken, FrameworkAgentRuntime, ModelStep, RunContext,
    RunJournal, ScriptedDriver,
};
use codypendent_runtime::models::ModelRegistry;
use codypendent_runtime::tools::{ArtifactSink, ClosureSink};
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
    let disposition = handle.await.unwrap().unwrap();

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
        .unwrap();

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
        .unwrap();

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
        .unwrap();
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

    let disposition = runtime.execute_run(&driver, ctx, token).await.unwrap();
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
        .unwrap();
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
        _number: u64,
        _req: &model::UpdatePullRequest,
    ) -> Result<model::PullRequest, GitHubError> {
        Err(unused_github_error())
    }

    async fn create_check_run_summary(
        &self,
        _repo: &RepoId,
        _req: &model::NewCheckRun,
    ) -> Result<model::CheckRun, GitHubError> {
        Err(unused_github_error())
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
