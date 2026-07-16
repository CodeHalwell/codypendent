//! Startup recovery and the failure matrix (STEP 1.14).
//!
//! Seeds a crash's aftermath directly with raw `sqlx` INSERTs (a `sessions` row,
//! a `runs` row in a live state, a `pending_effects` row, a stale
//! `workspace_leases` row), runs [`recovery::recover_on_startup`], and asserts the
//! documented restart state: a live run ends cleanly `Failed` with a retrievable
//! chronicle, an in-flight effect is reconciled exactly once, a stale lease is
//! `orphaned`, and a second recovery pass is a no-op.
//!
//! The real-crash test (spawn the `codypendentd` binary, `kill -9`, restart, and
//! assert the parked run recovered to `Failed`) lives in the assembly crate that
//! builds that binary — `crates/codypendentd/tests/recovery_it.rs` — because
//! `CARGO_BIN_EXE_codypendentd` is only defined for the crate that owns the bin.

use chrono::Utc;
use codypendent_daemon::artifacts::ArtifactStore;
use codypendent_daemon::{db, ledger, projections, recovery};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{CommandId, EventBody, RunDisposition, RunId, RunState, SessionId};
use sqlx::SqlitePool;
use tokio::io::AsyncReadExt;
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
