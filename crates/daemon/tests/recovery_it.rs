//! Startup recovery and the failure matrix (STEP 1.14).
//!
//! Seeds a crash's aftermath directly with raw `sqlx` INSERTs (a `sessions` row,
//! a `runs` row in a live state, a `pending_effects` row, a stale
//! `workspace_leases` row), runs [`recovery::recover_on_startup`], and asserts the
//! documented restart state: a live run ends cleanly `Failed` with a retrievable
//! chronicle, an in-flight effect is reconciled exactly once, a stale lease is
//! `orphaned`, and a second recovery pass is a no-op.
//!
//! The final test is the real crash: it spawns the `codypendentd` binary against
//! a temp data dir, creates and parks a run over the socket, `kill -9`s the
//! child, restarts it, and asserts the run recovered to `Failed` — exercising the
//! `main.rs` startup wiring end to end.

use std::path::Path;
use std::process::{Child, Command as StdCommand, Stdio};
use std::str::FromStr;
use std::time::Duration;

use chrono::Utc;
use codypendent_daemon::artifacts::ArtifactStore;
use codypendent_daemon::{db, ledger, projections, recovery};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    read_envelope, write_envelope, AgentMode, ClientCapabilities, ClientHello, ClientId,
    Command as ProtoCommand, CommandBody, CommandId, Envelope, EventBody, Payload, RunDisposition,
    RunId, RunState, SessionId, WorkspaceId, PROTOCOL_V1,
};
use sqlx::SqlitePool;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use uuid::Uuid;

/// A temp data dir plus a pool onto its database, ready for direct seeding.
async fn setup(tmp: &tempfile::TempDir) -> (RuntimePaths, SqlitePool) {
    let paths = RuntimePaths::from_data_dir(tmp.path().to_path_buf());
    paths.ensure_directories().expect("create directories");
    let pool = db::open_database(&paths.data_dir.join("codypendent.db"))
        .await
        .expect("open db");
    (paths, pool)
}

/// Seed a `runs` row in the given state (PascalCase, as the projections store it)
/// and return its id. A `sessions` row for `session` must already exist.
async fn seed_run(pool: &SqlitePool, session: SessionId, state: &str, objective: &str) -> RunId {
    let run = RunId::new();
    sqlx::query(
        "INSERT INTO runs (id, session_id, objective, state, mode, model_policy, budget_json) \
         VALUES (?, ?, ?, ?, 'Build', 'hosted-default', '{}')",
    )
    .bind(run.to_string())
    .bind(session.to_string())
    .bind(objective)
    .bind(state)
    .execute(pool)
    .await
    .expect("insert run");
    run
}

#[tokio::test]
async fn live_run_at_boot_is_cleanly_failed_with_chronicle() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, pool) = setup(&tmp).await;

    let session = SessionId::new();
    ledger::create_session(&pool, session, "diagnose")
        .await
        .unwrap();
    let run = seed_run(&pool, session, "Running", "fix the parser").await;

    let report = recovery::recover_on_startup(&pool, &paths).await.unwrap();
    assert_eq!(report.failed_runs, vec![run]);

    // The projection row ends `Failed`.
    assert_eq!(
        projections::load_run_state(&pool, run).await.unwrap(),
        Some(RunState::Failed),
    );

    // The ledger records the run moving through `Recovering`, then a terminal
    // `RunCompleted { Failed }` carrying a chronicle `ArtifactRef`.
    let events = ledger::load_events(&pool, session).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(
            &e.body,
            EventBody::RunStateChanged { run_id, state: RunState::Recovering } if *run_id == run
        )),
        "a Recovering transition must be recorded",
    );
    let chronicle_ref = events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Failed { reason },
                chronicle,
            } if *run_id == run => {
                assert_eq!(reason, "daemon restart");
                Some(chronicle.clone())
            }
            _ => None,
        })
        .expect("RunCompleted with a Failed disposition and chronicle");

    // The chronicle artifact is retrievable and describes the run.
    let store = ArtifactStore::new(paths.data_dir.join("artifacts"));
    let mut file = store.open(&pool, chronicle_ref.id).await.unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["objective"], "fix the parser");
    assert_eq!(json["disposition"], "Failed");
}

#[tokio::test]
async fn pending_effect_is_reconciled_without_duplicate() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, pool) = setup(&tmp).await;

    let session = SessionId::new();
    ledger::create_session(&pool, session, "effects")
        .await
        .unwrap();

    // A command left `received` with an `intended` effect that never ran — the
    // crash-between-persist-and-effect injection point.
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

    let effect_id = Uuid::now_v7().to_string();
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

    let report = recovery::recover_on_startup(&pool, &paths).await.unwrap();
    assert!(report.reconciled_effects >= 1);

    // The effect resolved exactly once, and no duplicate effect appeared.
    let (state,): (String,) = sqlx::query_as("SELECT state FROM pending_effects WHERE id = ?")
        .bind(&effect_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        state == "reconciled" || state == "abandoned",
        "unexpected effect state {state}"
    );
    let (rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pending_effects")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "no duplicate effect row");
}

#[tokio::test]
async fn stale_worktree_lease_is_orphaned() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, pool) = setup(&tmp).await;

    let session = SessionId::new();
    ledger::create_session(&pool, session, "leases")
        .await
        .unwrap();
    // A terminal run so the recovery run-sweep leaves it alone; it exists only to
    // satisfy the lease's `owner_run_id` foreign key.
    let run = seed_run(&pool, session, "Completed", "done").await;

    let lease_id = Uuid::now_v7();
    let missing = tmp.path().join("gone").join("run-x");
    sqlx::query(
        "INSERT INTO workspace_leases \
         (id, repository_path, worktree_path, branch, base_commit, owner_run_id, mode, state, \
          created_at, expires_at) \
         VALUES (?, ?, ?, ?, ?, ?, 'write', 'active', ?, ?)",
    )
    .bind(lease_id.to_string())
    .bind(tmp.path().join("repo").to_string_lossy().as_ref())
    .bind(missing.to_string_lossy().as_ref())
    .bind("codypendent/run-x")
    .bind("0".repeat(40))
    .bind(run.to_string())
    .bind(Utc::now().to_rfc3339())
    .bind(Utc::now().to_rfc3339())
    .execute(&pool)
    .await
    .unwrap();

    let report = recovery::recover_on_startup(&pool, &paths).await.unwrap();
    assert!(
        report.orphaned_leases.contains(&lease_id),
        "the stale lease must be reported orphaned"
    );

    let (state,): (String,) = sqlx::query_as("SELECT state FROM workspace_leases WHERE id = ?")
        .bind(lease_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "orphaned");
    assert!(
        !missing.exists(),
        "recovery must not create the missing dir"
    );
}

#[tokio::test]
async fn recovery_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, pool) = setup(&tmp).await;

    let session = SessionId::new();
    ledger::create_session(&pool, session, "idem")
        .await
        .unwrap();
    let run = seed_run(&pool, session, "Running", "keep going").await;

    // A stray tmp file simulating crash garbage the sweep must remove.
    let tmp_dir = paths.data_dir.join("artifacts").join("tmp");
    tokio::fs::create_dir_all(&tmp_dir).await.unwrap();
    tokio::fs::write(tmp_dir.join("leftover-from-crash"), b"garbage")
        .await
        .unwrap();

    let first = recovery::recover_on_startup(&pool, &paths).await.unwrap();
    assert_eq!(first.failed_runs, vec![run]);
    assert_eq!(first.swept_tmp, 1);

    // Second pass: the run is already Failed (not live), tmp is already empty.
    let second = recovery::recover_on_startup(&pool, &paths).await.unwrap();
    assert!(
        second.failed_runs.is_empty(),
        "a Failed run is never re-failed"
    );
    assert_eq!(second.swept_tmp, 0, "tmp was already swept");

    // Exactly one terminal event for the run — no duplicate RunCompleted.
    let events = ledger::load_events(&pool, session).await.unwrap();
    let completed = events
        .iter()
        .filter(|e| matches!(&e.body, EventBody::RunCompleted { run_id, .. } if *run_id == run))
        .count();
    assert_eq!(completed, 1, "no duplicate RunCompleted");
}

// --- Real crash test: spawn the binary, kill -9, restart --------------------

/// Spawn the `codypendentd` binary against a temp data dir, with a quiet log and
/// discarded output. The socket resolves under `<data_dir>/run/` (the data-dir
/// override branch of discovery), matching what the test connects to.
fn spawn_daemon(data_dir: &Path) -> Child {
    StdCommand::new(env!("CARGO_BIN_EXE_codypendentd"))
        .env("CODYPENDENT_DATA_DIR", data_dir)
        .env_remove("CODYPENDENT_SOCKET")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn codypendentd")
}

/// Poll-connect until the daemon's socket accepts (its recovery has finished and
/// the listener is up), or panic after ~10s.
async fn wait_for_socket(paths: &RuntimePaths) -> UnixStream {
    for _ in 0..200 {
        if let Ok(stream) = UnixStream::connect(&paths.socket_path).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "daemon socket never came up at {}",
        paths.socket_path.display()
    );
}

async fn read_frame(stream: &mut UnixStream) -> Envelope {
    tokio::time::timeout(Duration::from_secs(5), read_envelope(stream))
        .await
        .expect("read timed out")
        .expect("read frame")
        .expect("server must reply")
}

/// Handshake, asserting a `ServerHello`. A handshaken local connection defaults
/// to the `Controller` role, so no explicit attach is needed to create/control.
async fn handshake(stream: &mut UnixStream, client: ClientId) {
    let hello = ClientHello {
        client_name: "recovery-it".to_string(),
        client_version: "0".to_string(),
        supported_protocols: vec![PROTOCOL_V1],
        capabilities: ClientCapabilities::default(),
        resume_token: None,
    };
    write_envelope(
        stream,
        &Envelope::request(client, Payload::ClientHello(hello)),
    )
    .await
    .expect("write hello");
    assert!(matches!(
        read_frame(stream).await.payload,
        Payload::ServerHello(_)
    ));
}

/// Send one command and return the first non-heartbeat reply envelope.
async fn send_command(
    stream: &mut UnixStream,
    client: ClientId,
    body: CommandBody,
    key: &str,
) -> Envelope {
    let cmd = ProtoCommand {
        command_id: CommandId::new(),
        idempotency_key: key.to_string(),
        expected_revision: None,
        body,
    };
    write_envelope(stream, &Envelope::request(client, Payload::Command(cmd)))
        .await
        .expect("write command");
    loop {
        let env = read_frame(stream).await;
        if matches!(env.payload, Payload::Ping) {
            continue;
        }
        return env;
    }
}

async fn open_pool(paths: &RuntimePaths) -> SqlitePool {
    db::open_database(&paths.data_dir.join("codypendent.db"))
        .await
        .expect("open db")
}

/// Poll (against a short-lived read pool) for the single run in `session`.
async fn wait_for_run(paths: &RuntimePaths, session: SessionId) -> RunId {
    let pool = open_pool(paths).await;
    let mut found = None;
    for _ in 0..100 {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT id FROM runs WHERE session_id = ? LIMIT 1")
                .bind(session.to_string())
                .fetch_optional(&pool)
                .await
                .unwrap();
        if let Some((id,)) = row {
            found = Some(RunId::from_str(&id).unwrap());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    pool.close().await;
    found.expect("run row appeared")
}

#[tokio::test]
async fn kill9_daemon_recovers_parked_run_to_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    let paths = RuntimePaths::from_data_dir(data_dir.clone());

    // Boot the real daemon and drive it over the socket.
    let mut child = spawn_daemon(&data_dir);
    let mut stream = wait_for_socket(&paths).await;
    let client = ClientId::new();
    handshake(&mut stream, client).await;

    // Create a session (its id rides back on the envelope), start a run, then
    // pause it — `Paused` is a live state, the only one reachable over the socket
    // without the agent loop (STEP 1.10), so recovery must fail it.
    let create = send_command(
        &mut stream,
        client,
        CommandBody::CreateSession {
            workspace: WorkspaceId::new(),
            title: "diagnose".to_string(),
        },
        "create",
    )
    .await;
    let session = create.session_id.expect("created session id on envelope");

    let started = send_command(
        &mut stream,
        client,
        CommandBody::StartRun {
            session_id: session,
            objective: "diagnose".to_string(),
            mode: AgentMode::Build,
        },
        "start",
    )
    .await;
    assert!(matches!(started.payload, Payload::CommandAccepted { .. }));

    let run = wait_for_run(&paths, session).await;
    let paused = send_command(
        &mut stream,
        client,
        CommandBody::PauseRun { run_id: run },
        "pause",
    )
    .await;
    assert!(matches!(paused.payload, Payload::CommandAccepted { .. }));
    drop(stream);

    // Crash the daemon uncleanly.
    let _ = child.kill();
    let _ = child.wait();

    // Restart: recovery runs before the socket reopens.
    let mut child2 = spawn_daemon(&data_dir);
    let _stream2 = wait_for_socket(&paths).await;

    // The parked run recovered to Failed, with a RunCompleted terminal event.
    let pool = open_pool(&paths).await;
    let mut final_state = None;
    for _ in 0..100 {
        if let Some(state) = projections::load_run_state(&pool, run).await.unwrap() {
            if state == RunState::Failed {
                final_state = Some(state);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        final_state,
        Some(RunState::Failed),
        "the parked run must recover to Failed after kill -9"
    );

    let events = ledger::load_events(&pool, session).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(
            &e.body,
            EventBody::RunCompleted { run_id, disposition: RunDisposition::Failed { .. }, .. }
                if *run_id == run
        )),
        "a RunCompleted(Failed) must be recorded for the recovered run"
    );
    pool.close().await;

    let _ = child2.kill();
    let _ = child2.wait();
}
