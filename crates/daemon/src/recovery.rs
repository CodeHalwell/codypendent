//! Startup recovery and the failure matrix (STEP 1.14).
//!
//! Before the socket opens, the daemon reconciles the durable state a previous
//! process may have left mid-flight. [`recover_on_startup`] runs, in order:
//!
//! 1. **Artifact tmp-sweep** — [`ArtifactStore::sweep_tmp`] deletes the `tmp/`
//!    garbage a crash leaves between a streamed write and its atomic rename
//!    (STEP 1.4 RULE 2).
//! 2. **Worktree reconciliation** — [`WorktreeManager::reconcile_on_startup`]
//!    marks leases whose directory has vanished `orphaned` and flags stray
//!    tracked worktrees; it never deletes (STEP 1.8).
//! 3. **Pending-effect reconciliation** — [`CommandProcessor::reconcile_pending_effects`]
//!    sweeps `pending_effects` still `intended`/`performed` from a crash mid-apply
//!    so a duplicate external effect can never be re-performed (STEP 1.3 RULE 4).
//! 4. **Run recovery** — every run in a *live* state at boot ([`is_live`]) is
//!    ended cleanly. Phase 1 keeps no mid-node checkpoint, so a live run cannot be
//!    resumed; it is transitioned through `Recovering` and finished as `Failed`
//!    with a chronicle artifact and its existing artifacts intact. The *only*
//!    forbidden outcome is silent disappearance — "recovers or cleanly marks the
//!    run" is the Phase 1 exit criterion.
//! 5. **Approval re-surfacing** — [`ApprovalBroker::reload_pending`] re-loads the
//!    `pending` approvals so newly attached clients see them again (STEP 1.6).
//!
//! Recovery is **idempotent**: a run already `Failed` is not a live run, so it is
//! never re-failed; a swept `tmp/` is already empty; reconciled effects are no
//! longer `intended`. Running it twice changes nothing the second time.

use std::path::Path;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    Actor, ApprovalId, DataClassification, EventBody, RunDisposition, RunId, RunState,
    SessionEvent, SessionId,
};
use serde::Serialize;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::approvals::ApprovalBroker;
use crate::artifacts::{ArtifactStore, Provenance};
use crate::commands::CommandProcessor;
use crate::projections::{self, run_state_from_db};
use crate::subscriptions::SubscriptionHub;
use crate::worktrees::WorktreeManager;

/// The `RunDisposition::Failed` reason recorded on a run failed by restart
/// recovery.
const RESTART_REASON: &str = "daemon restart";

/// What [`recover_on_startup`] did, for the boot log and for tests. Every field
/// is empty/zero on a clean boot and on the idempotent second pass.
#[derive(Debug, Clone, Default)]
pub struct RecoveryReport {
    /// Number of stray files/dirs removed from the artifact store's `tmp/`.
    pub swept_tmp: usize,
    /// Lease ids whose worktree directory was missing; marked `orphaned`.
    pub orphaned_leases: Vec<Uuid>,
    /// Number of `pending_effects` reconciled (marked `reconciled`/`abandoned`).
    pub reconciled_effects: usize,
    /// Runs that were live at boot and were cleanly failed with a chronicle.
    pub failed_runs: Vec<RunId>,
    /// Pending approvals re-surfaced for newly attached clients.
    pub resurfaced_approvals: Vec<ApprovalId>,
}

/// Whether a run state is *live* — in flight when the daemon stopped, and so a
/// candidate for recovery. The Phase 1 live set (STEP 1.14): a run that had begun
/// but not reached a terminal state. `Queued` is excluded (never started; simply
/// picked up), as are the terminal states.
pub fn is_live(state: RunState) -> bool {
    matches!(
        state,
        RunState::Running
            | RunState::Preparing
            | RunState::WaitingForApproval
            | RunState::WaitingForUserInput
            | RunState::Paused
            | RunState::Recovering
    )
}

/// Reconcile durable state a previous daemon left mid-flight, then return a
/// summary. Runs **before** the socket opens (wired into `main.rs` after
/// `record_boot`), so no client can observe a half-recovered run.
pub async fn recover_on_startup(
    pool: &SqlitePool,
    paths: &RuntimePaths,
) -> anyhow::Result<RecoveryReport> {
    // 1. Sweep artifact-store tmp garbage. Count the pre-existing entries first so
    //    the report reflects crash garbage only (the chronicle writes in step 4
    //    rename out of tmp cleanly).
    let artifacts_root = paths.data_dir.join("artifacts");
    let artifacts = ArtifactStore::new(artifacts_root.clone());
    let swept_tmp = count_tmp_entries(&artifacts_root).await;
    artifacts.sweep_tmp().await?;

    // 2. Reconcile worktree leases against Git (never deletes on startup).
    let reconcile = WorktreeManager::new().reconcile_on_startup(pool).await?;
    let orphaned_leases = reconcile.orphaned_leases;

    // 3. Reconcile in-flight pending effects (a fresh, throwaway processor: the
    //    real one is built in `server::run`; recovery only needs the sweep).
    let processor = CommandProcessor::new(SubscriptionHub::new(), ApprovalBroker::new());
    let reconciled_effects = processor.reconcile_pending_effects(pool).await?;

    // 4. Cleanly fail every live run (no mid-node checkpoint exists in Phase 1).
    let failed_runs = recover_live_runs(pool, &artifacts).await?;

    // 5. Re-surface pending approvals for clients that re-attach.
    let resurfaced_approvals = ApprovalBroker::new()
        .reload_pending(pool)
        .await?
        .into_iter()
        .map(|approval| approval.approval_id)
        .collect();

    Ok(RecoveryReport {
        swept_tmp,
        orphaned_leases,
        reconciled_effects,
        failed_runs,
        resurfaced_approvals,
    })
}

/// The minimal chronicle stored for a run ended by restart recovery. A full
/// Chronicle v0 (findings, actions, verification, costs) is folded from a run's
/// own events by the agent loop (STEP 1.10); a run killed mid-flight has no such
/// terminal fold, so recovery records this abbreviated form — enough to attribute
/// the failure and point at the run's last durable sequence.
#[derive(Debug, Serialize)]
struct RecoveryChronicle {
    run_id: RunId,
    objective: String,
    /// The terminal kind, always `"Failed"` here.
    disposition: String,
    /// Human-readable cause.
    summary: String,
    /// The run's last durable event sequence before recovery touched the ledger.
    last_sequence: u64,
    recovered_at: DateTime<Utc>,
}

/// Fail every live run in the `runs` table, returning the ids failed. Runs are
/// filtered in Rust via [`is_live`] (a single source of truth, forward-compatible
/// with new states) rather than an SQL `IN` list.
async fn recover_live_runs(
    pool: &SqlitePool,
    artifacts: &ArtifactStore,
) -> anyhow::Result<Vec<RunId>> {
    let rows: Vec<(String, String, String, String)> =
        sqlx::query_as("SELECT id, session_id, objective, state FROM runs")
            .fetch_all(pool)
            .await?;

    let mut failed = Vec::new();
    for (id, session, objective, state) in rows {
        if !is_live(run_state_from_db(&state)) {
            continue;
        }
        let run_id = RunId::from_str(&id)?;
        let session_id = SessionId::from_str(&session)?;
        fail_live_run(pool, artifacts, run_id, session_id, &objective).await?;
        failed.push(run_id);
    }
    Ok(failed)
}

/// End one live run cleanly: record it moving through `Recovering`, store a
/// chronicle, and append the terminal `RunCompleted { Failed }` that references
/// it — leaving the projection row `Failed`.
///
/// The chronicle is written to the artifact store *before* the failing
/// transaction (its `put` runs its own commit and cannot join our tx). This is
/// crash-safe: a crash after the `put` but before the tx commit leaves the run
/// still live, so the next recovery re-fails it and writes a fresh chronicle —
/// the CAS store dedups identical blobs, and an unreferenced artifact row is
/// harmless. What matters is atomicity of the *failing itself*: the `Recovering`
/// marker, the projection flip to `Failed`, and the `RunCompleted` event all
/// commit together, so a live run never lands half-failed.
async fn fail_live_run(
    pool: &SqlitePool,
    artifacts: &ArtifactStore,
    run_id: RunId,
    session_id: SessionId,
    objective: &str,
) -> anyhow::Result<()> {
    // The run's last durable sequence, before recovery appends anything.
    let last_sequence = crate::ledger::next_sequence(pool, session_id)
        .await?
        .saturating_sub(1);

    let chronicle = RecoveryChronicle {
        run_id,
        objective: objective.to_string(),
        disposition: "Failed".to_string(),
        summary: "ended by daemon restart recovery".to_string(),
        last_sequence,
        recovered_at: Utc::now(),
    };
    let chronicle_ref = artifacts
        .put(
            pool,
            "application/json",
            DataClassification::Internal,
            Provenance::system(format!("recovery-chronicle:{run_id}")),
            &serde_json::to_vec(&chronicle)?,
        )
        .await?;

    // One transaction ends the run. Sequences are allocated inside it, the
    // approvals/commands atomic-append pattern.
    let now = Utc::now().to_rfc3339();
    let mut tx = pool.begin().await?;

    let seq = next_sequence(&mut *tx, session_id).await?;
    append_event(
        &mut *tx,
        session_id,
        seq,
        &Actor::System,
        &EventBody::RunStateChanged {
            run_id,
            state: RunState::Recovering,
        },
        &now,
    )
    .await?;

    projections::set_run_state(&mut *tx, run_id, RunState::Failed).await?;

    let seq = next_sequence(&mut *tx, session_id).await?;
    append_event(
        &mut *tx,
        session_id,
        seq,
        &Actor::System,
        &EventBody::RunCompleted {
            run_id,
            disposition: RunDisposition::Failed {
                reason: RESTART_REASON.to_string(),
            },
            chronicle: chronicle_ref,
        },
        &now,
    )
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Fail a run cleanly to a terminal `Failed` state — persisting a chronicle and
/// both the `RunStateChanged { Failed }` and `RunCompleted { Failed }` events in
/// one transaction — then **publish** those events to `subscriptions` so an
/// attached client observes the terminal transition live.
///
/// Used by the assembly binary's run executor when a run cannot be executed
/// (most commonly: no model is configured or reachable). The point is that the
/// run reaches a TERMINAL state — never left `Queued`/`Running` — so a headless
/// `codypendent run --jsonl` stops waiting instead of hanging.
///
/// Unlike [`fail_live_run`] (startup recovery, which runs *before* the socket
/// opens and so has no subscribers, and routes a mid-flight run through
/// `Recovering`), this is a *live* failure: it transitions straight to `Failed`
/// and publishes, mirroring the agent loop's own terminal path
/// (persist-before-publish).
pub async fn fail_run(
    pool: &SqlitePool,
    artifacts: &ArtifactStore,
    subscriptions: &SubscriptionHub,
    run_id: RunId,
    session_id: SessionId,
    objective: &str,
    reason: &str,
) -> anyhow::Result<()> {
    // The run's last durable sequence, before this failure appends anything.
    let last_sequence = crate::ledger::next_sequence(pool, session_id)
        .await?
        .saturating_sub(1);

    let chronicle = RecoveryChronicle {
        run_id,
        objective: objective.to_string(),
        disposition: "Failed".to_string(),
        summary: reason.to_string(),
        last_sequence,
        recovered_at: Utc::now(),
    };
    // The chronicle blob is written before the failing transaction (its `put`
    // runs its own commit); an unreferenced blob after a crash is harmless.
    let chronicle_ref = artifacts
        .put(
            pool,
            "application/json",
            DataClassification::Internal,
            Provenance::system(format!("run-failed:{run_id}")),
            &serde_json::to_vec(&chronicle)?,
        )
        .await?;

    // One transaction: the projection flip to `Failed` and both terminal events
    // commit together, so a run never lands half-failed.
    let now = Utc::now();
    let now_str = now.to_rfc3339();
    let mut tx = pool.begin().await?;

    let failed_state = EventBody::RunStateChanged {
        run_id,
        state: RunState::Failed,
    };
    let seq1 = next_sequence(&mut *tx, session_id).await?;
    append_event(
        &mut *tx,
        session_id,
        seq1,
        &Actor::System,
        &failed_state,
        &now_str,
    )
    .await?;

    projections::set_run_state(&mut *tx, run_id, RunState::Failed).await?;

    let completed = EventBody::RunCompleted {
        run_id,
        disposition: RunDisposition::Failed {
            reason: reason.to_string(),
        },
        chronicle: chronicle_ref,
    };
    let seq2 = next_sequence(&mut *tx, session_id).await?;
    append_event(
        &mut *tx,
        session_id,
        seq2,
        &Actor::System,
        &completed,
        &now_str,
    )
    .await?;

    tx.commit().await?;

    // Persist-before-publish: only after the commit do the terminal events fan
    // out to any attached client. Publishing to zero subscribers is normal.
    subscriptions.publish(
        session_id,
        SessionEvent {
            sequence: u64::try_from(seq1)?,
            occurred_at: now,
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body: failed_state,
        },
    );
    subscriptions.publish(
        session_id,
        SessionEvent {
            sequence: u64::try_from(seq2)?,
            occurred_at: now,
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body: completed,
        },
    );
    Ok(())
}

/// Count the top-level entries under `<artifacts>/tmp` (missing dir ⇒ 0). Read
/// before the sweep so the report reflects only crash garbage.
async fn count_tmp_entries(artifacts_root: &Path) -> usize {
    let tmp_dir = artifacts_root.join("tmp");
    let mut entries = match tokio::fs::read_dir(&tmp_dir).await {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    let mut count = 0usize;
    while let Ok(Some(_entry)) = entries.next_entry().await {
        count += 1;
    }
    count
}

/// The next 1-based event sequence for a session, read inside the caller's
/// transaction so the append that claims it is atomic with the read (mirrors
/// [`crate::approvals`] / [`crate::commands`]).
async fn next_sequence(
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

/// Append one event within the caller's transaction (`System` actor, no
/// causation — recovery housekeeping, like the approval broker's own events).
async fn append_event(
    exec: impl sqlx::SqliteExecutor<'_>,
    session_id: SessionId,
    sequence: i64,
    actor: &Actor,
    body: &EventBody,
    occurred_at: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO events \
         (session_id, sequence, occurred_at, actor, body, causation_id, correlation_id, schema_version) \
         VALUES (?, ?, ?, ?, ?, NULL, NULL, 1)",
    )
    .bind(session_id.to_string())
    .bind(sequence)
    .bind(occurred_at)
    .bind(serde_json::to_string(actor)?)
    .bind(serde_json::to_string(body)?)
    .execute(exec)
    .await?;
    Ok(())
}
