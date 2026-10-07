//! Integration tests for daemon control plane synchronization.

use std::sync::Arc;

use axum::{
    extract::Query,
    http::{header::AUTHORIZATION, HeaderMap},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use codypendent_control_plane_protocol::{
    DataClassification, FederatedRepositoryId, OrganizationId, PolicyRestrictions, PolicySnapshot,
    PolicyUpdateEvent, PublicationClass, Repository as ControlPlaneRepository, RepositoryId,
    Sha256Digest, StreamEvent, StreamEventPayload, StreamKind, SyncRejection,
};
use codypendent_daemon::{
    artifacts::{ArtifactStore, Provenance},
    control_plane_sync::{
        acknowledge_receipt, compute_effective_policy, enqueue_artifact_summary, enqueue_delta,
        enqueue_run_summary, enqueue_session_summary, enqueue_tombstone, fetch_pending_deltas,
        get_pairing, get_policy_snapshot, get_repository_stream_cursor, get_stream_cursor,
        has_inbound_receipt, list_active_pairings, reconcile_authoritative_writes,
        reconcile_authoritative_writes_for_pairing, record_inbound_receipt, record_pairing,
        revoke_pairing, set_stream_cursor, store_policy_snapshot, ControlPlaneCredential,
        ControlPlanePairing, ControlPlaneSyncError, InboundReceipt, LocalConsentManifest,
        PairingState, SyncDeltaPushRequest, SyncDeltaPushResponse, SyncEngine,
    },
    db, projections,
};
use codypendent_protocol::{DataClassification as ArtifactClassification, RunState};
use tokio::net::TcpListener;
use uuid::Uuid;

async fn setup_test_db() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("codypendent-test.db");
    let pool = db::open_database(&db_path).await.expect("open db");
    (tmp, pool)
}

fn catalog_repository(
    organization_id: Uuid,
    repository_id: Uuid,
    federated_id: &str,
) -> ControlPlaneRepository {
    ControlPlaneRepository {
        id: RepositoryId::from_uuid(repository_id),
        organization_id: OrganizationId::from_uuid(organization_id),
        federated_id: FederatedRepositoryId::new(federated_id)
            .expect("valid federated repository id"),
        display_name: "Mock repository".to_string(),
        max_publication_class: PublicationClass::MetadataShared,
        max_classification: DataClassification::Internal,
        policy_version: 1,
        created_at: Utc::now(),
    }
}

#[tokio::test]
async fn pairing_lifecycle_and_consent_manifest() {
    let (_tmp, pool) = setup_test_db().await;

    let manifest = LocalConsentManifest {
        organization_id: "org_123".to_string(),
        organization_display_name: "Acme Corp".to_string(),
        endpoint: "https://control-plane.acme.corp".to_string(),
        max_publication_class: PublicationClass::MetadataShared,
        accepts_remote_approvals: false,
        accepts_runner_dispatch: false,
        allowed_repositories: vec!["repo_abc".to_string()],
        created_at: Utc::now(),
    };

    let manifest_json = serde_json::to_string(&manifest).unwrap();
    let manifest_hash = manifest.compute_hash();
    assert_eq!(manifest_hash.len(), 64);

    let pairing_id = Uuid::now_v7().to_string();
    let pairing = ControlPlanePairing {
        id: pairing_id.clone(),
        owner_uid: 501,
        endpoint: "https://control-plane.acme.corp".to_string(),
        organization_id: "org_123".to_string(),
        organization_display_name: "Acme Corp".to_string(),
        consent_manifest: manifest_json,
        consent_manifest_hash: manifest_hash,
        max_publication_class: PublicationClass::MetadataShared,
        accepts_remote_approvals: false,
        accepts_runner_dispatch: false,
        state: PairingState::Active,
        paired_at: Some(Utc::now()),
        expires_at: None,
        revoked_at: None,
        revoked_reason: None,
        created_at: Utc::now(),
    };

    let cred = ControlPlaneCredential {
        pairing_id: pairing_id.clone(),
        credential_ref: format!("keychain:codypendent.control-plane.{pairing_id}"),
        credential_hash: "0000000000000000000000000000000000000000000000000000000000000000"
            .to_string(),
        audience: "https://control-plane.acme.corp".to_string(),
        purpose: "sync".to_string(),
        issued_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::days(30),
        rotated_at: None,
    };

    record_pairing(&pool, &pairing, &cred).await.unwrap();

    let fetched = get_pairing(&pool, &pairing_id)
        .await
        .unwrap()
        .expect("pairing exists");
    assert_eq!(fetched.id, pairing_id);
    assert_eq!(fetched.state, PairingState::Active);
    assert_eq!(
        fetched.max_publication_class,
        PublicationClass::MetadataShared
    );

    let active = list_active_pairings(&pool).await.unwrap();
    assert_eq!(active.len(), 1);

    // Revoke pairing
    revoke_pairing(&pool, &pairing_id, "user revoked access")
        .await
        .unwrap();
    let revoked = get_pairing(&pool, &pairing_id).await.unwrap().unwrap();
    assert_eq!(revoked.state, PairingState::Revoked);
    assert_eq!(
        revoked.revoked_reason.as_deref(),
        Some("user revoked access")
    );

    let active_after = list_active_pairings(&pool).await.unwrap();
    assert_eq!(active_after.len(), 0);
}

#[tokio::test]
async fn outbox_enqueue_redaction_and_monotonic_sequence() {
    let (_tmp, pool) = setup_test_db().await;
    let pairing_id = Uuid::now_v7().to_string();

    let pairing = ControlPlanePairing {
        id: pairing_id.clone(),
        owner_uid: 501,
        endpoint: "https://control-plane.test".to_string(),
        organization_id: "org_test".to_string(),
        organization_display_name: "Test Org".to_string(),
        consent_manifest: "{}".to_string(),
        consent_manifest_hash: "1111111111111111111111111111111111111111111111111111111111111111"
            .to_string(),
        max_publication_class: PublicationClass::MetadataShared,
        accepts_remote_approvals: false,
        accepts_runner_dispatch: false,
        state: PairingState::Active,
        paired_at: Some(Utc::now()),
        expires_at: None,
        revoked_at: None,
        revoked_reason: None,
        created_at: Utc::now(),
    };
    let cred = ControlPlaneCredential {
        pairing_id: pairing_id.clone(),
        credential_ref: "keychain:test".to_string(),
        credential_hash: "2222222222222222222222222222222222222222222222222222222222222222"
            .to_string(),
        audience: "https://control-plane.test".to_string(),
        purpose: "sync".to_string(),
        issued_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::days(1),
        rotated_at: None,
    };
    record_pairing(&pool, &pairing, &cred).await.unwrap();

    // 1. Session summary with requested class ContentShared, but pairing max is MetadataShared
    let _outbox_id1 = enqueue_session_summary(
        &pool,
        &pairing_id,
        pairing.max_publication_class,
        "sess_001",
        Some("repo_001"),
        "completed",
        Utc::now(),
        Some(Utc::now()),
        Some("Top Secret Title"),
        PublicationClass::ContentShared,
    )
    .await
    .unwrap()
    .expect("enqueued delta 1");

    // 2. PrivateLocal data must NOT be enqueued (returns None)
    let outbox_id_private = enqueue_session_summary(
        &pool,
        &pairing_id,
        pairing.max_publication_class,
        "sess_private",
        Some("repo_001"),
        "active",
        Utc::now(),
        None,
        Some("Local Title"),
        PublicationClass::PrivateLocal,
    )
    .await
    .unwrap();
    assert!(
        outbox_id_private.is_none(),
        "PrivateLocal delta must not be enqueued to outbox"
    );

    // 3. Enqueue run summary, artifact summary, graph batch, audit event, tombstone
    // Captured so the duplicate below can replay the IDENTICAL payload: the
    // outbox deduplicates on `payload_hash`, so fresh timestamps would make it
    // a genuinely different delta rather than a duplicate.
    let run_started_at = Utc::now();
    let run_completed_at = Utc::now();
    let _outbox_id2 = enqueue_run_summary(
        &pool,
        &pairing_id,
        pairing.max_publication_class,
        "run_001",
        "sess_001",
        Some("repo_001"),
        "succeeded",
        run_started_at,
        Some(run_completed_at),
        Some("ok"),
        PublicationClass::MetadataShared,
    )
    .await
    .unwrap()
    .expect("enqueued delta 2");

    let _outbox_id3 = enqueue_artifact_summary(
        &pool,
        &pairing_id,
        pairing.max_publication_class,
        "art_001",
        Some("repo_001"),
        "report.pdf",
        "sha256:abcd",
        1024,
        "application/pdf",
        PublicationClass::MetadataShared,
    )
    .await
    .unwrap()
    .expect("enqueued delta 3");

    let _outbox_id4 = enqueue_tombstone(
        &pool,
        &pairing_id,
        pairing.max_publication_class,
        "repo_001",
        "session",
        "sess_old",
        "deleted",
    )
    .await
    .unwrap()
    .expect("enqueued delta 4");

    // Verify pending deltas
    let pending = fetch_pending_deltas(&pool, &pairing_id, 10).await.unwrap();
    assert_eq!(pending.len(), 4);

    // Verify sequence monotonicity (1, 2, 3, 4)
    assert_eq!(pending[0].sequence, 1);
    assert_eq!(pending[1].sequence, 2);
    assert_eq!(pending[2].sequence, 3);
    assert_eq!(pending[3].sequence, 4);

    // Verify redaction at enqueue time: session title is NULL under metadata-shared
    assert_eq!(pending[0].class, PublicationClass::MetadataShared);
    assert_eq!(pending[0].payload["title"], serde_json::Value::Null);

    // Verify duplicate enqueue is deduplicated safely
    let dup_id = enqueue_run_summary(
        &pool,
        &pairing_id,
        pairing.max_publication_class,
        "run_001",
        "sess_001",
        Some("repo_001"),
        "succeeded",
        run_started_at,
        Some(run_completed_at),
        Some("ok"),
        PublicationClass::MetadataShared,
    )
    .await
    .unwrap();
    assert!(dup_id.is_none(), "Duplicate outbox item should be ignored");
}

async fn seed_publication_pairing(
    pool: &sqlx::SqlitePool,
    owner_uid: u32,
    allowed_repository: &str,
    max_class: PublicationClass,
) -> String {
    let pairing_id = Uuid::now_v7().to_string();
    let organization_id = format!("org_{pairing_id}");
    let endpoint = format!("https://{pairing_id}.control-plane.test");
    let manifest = LocalConsentManifest {
        organization_id,
        organization_display_name: "Reconcile Org".to_string(),
        endpoint,
        max_publication_class: max_class,
        accepts_remote_approvals: false,
        accepts_runner_dispatch: false,
        allowed_repositories: vec![allowed_repository.to_string()],
        created_at: Utc::now(),
    };
    let pairing = ControlPlanePairing {
        id: pairing_id.clone(),
        owner_uid,
        endpoint: manifest.endpoint.clone(),
        organization_id: manifest.organization_id.clone(),
        organization_display_name: manifest.organization_display_name.clone(),
        consent_manifest: serde_json::to_string(&manifest).unwrap(),
        consent_manifest_hash: manifest.compute_hash(),
        max_publication_class: max_class,
        accepts_remote_approvals: false,
        accepts_runner_dispatch: false,
        state: PairingState::Active,
        paired_at: Some(Utc::now()),
        expires_at: None,
        revoked_at: None,
        revoked_reason: None,
        created_at: Utc::now(),
    };
    let credential = ControlPlaneCredential {
        pairing_id: pairing_id.clone(),
        credential_ref: format!("keychain:{pairing_id}"),
        credential_hash: "abababababababababababababababababababababababababababababababab"
            .to_string(),
        audience: "control-plane".to_string(),
        purpose: "sync".to_string(),
        issued_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::days(1),
        rotated_at: None,
    };
    record_pairing(pool, &pairing, &credential).await.unwrap();
    pairing_id
}

#[tokio::test]
async fn startup_reconciliation_repairs_generic_writes_without_graph_policy_and_is_idempotent() {
    let (_tmp, pool) = setup_test_db().await;
    let repository_id = "legacy-local-repository";
    let pairing_id =
        seed_publication_pairing(&pool, 501, repository_id, PublicationClass::MetadataShared).await;
    let now = Utc::now().to_rfc3339();
    let session_id = "session-reconcile";
    let run_id = codypendent_protocol::RunId::new();
    let run_id_string = run_id.to_string();
    let artifact_id = "artifact-reconcile";
    let approval_id = "approval-reconcile";

    sqlx::query(
        "INSERT INTO sessions \
         (id, title, state, created_at, updated_at, revision, owner_uid, repository_id) \
         VALUES (?, 'Repair me', 'open', ?, ?, 0, 501, ?)",
    )
    .bind(session_id)
    .bind(&now)
    .bind(&now)
    .bind(repository_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO runs \
         (id, session_id, objective, state, mode, model_policy, budget_json, started_at) \
         VALUES (?, ?, 'repair', 'Running', 'Build', 'hosted-default', '{}', ?)",
    )
    .bind(&run_id_string)
    .bind(session_id)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();
    let provenance = serde_json::json!({
        "source": {"kind": "tool_output", "tool": "shell.run", "run_id": run_id_string},
        "observed_at": now,
    });
    sqlx::query(
        "INSERT INTO artifacts \
         (id, sha256, media_type, byte_length, classification, created_at, provenance_json) \
         VALUES (?, ?, 'text/plain', 4, 'Internal', ?, ?)",
    )
    .bind(artifact_id)
    .bind("cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd")
    .bind(&now)
    .bind(serde_json::to_string(&provenance).unwrap())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO artifacts \
         (id, sha256, media_type, byte_length, classification, created_at, provenance_json) \
         VALUES ('malformed-legacy-artifact', ?, 'text/plain', 1, 'Internal', ?, 'not-json')",
    )
    .bind("edededededededededededededededededededededededededededededededed")
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO approvals \
         (id, run_id, action_json, risk_json, capabilities_json, state, scope, \
          resolved_by, requested_at, resolved_at) \
         VALUES (?, ?, '{\"type\":\"ExecuteCommand\"}', '{}', '[]', 'approved', \
                 'once', 'user-501', ?, ?)",
    )
    .bind(approval_id)
    .bind(&run_id_string)
    .bind(&now)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(reconcile_authoritative_writes(&pool).await.unwrap(), 4);
    let pending = fetch_pending_deltas(&pool, &pairing_id, 20).await.unwrap();
    let kinds: std::collections::HashSet<_> = pending
        .iter()
        .map(|entry| entry.delta_kind.as_str())
        .collect();
    assert_eq!(
        kinds,
        std::collections::HashSet::from([
            "session-summary",
            "run-summary",
            "artifact-summary",
            "approval-decision",
        ])
    );
    let metadata_session = pending
        .iter()
        .find(|entry| entry.delta_kind == "session-summary")
        .unwrap();
    assert_eq!(metadata_session.class, PublicationClass::MetadataShared);
    assert!(metadata_session.payload["title"].is_null());
    let store = ArtifactStore::new(_tmp.path().join("production-artifacts"));
    let produced = store
        .put(
            &pool,
            "text/plain",
            ArtifactClassification::Internal,
            Provenance::tool_output("shell.run", run_id),
            b"production artifact",
        )
        .await
        .unwrap();
    let pending = fetch_pending_deltas(&pool, &pairing_id, 20).await.unwrap();
    assert!(pending.iter().any(|entry| {
        entry.delta_kind == "artifact-summary" && entry.subject_id == produced.id.to_string()
    }));

    for (state, revision, updated_at) in [
        (
            "closed",
            1_i64,
            (Utc::now() + chrono::Duration::seconds(1)).to_rfc3339(),
        ),
        (
            "open",
            2_i64,
            (Utc::now() + chrono::Duration::seconds(2)).to_rfc3339(),
        ),
    ] {
        sqlx::query("UPDATE sessions SET state = ?, revision = ?, updated_at = ? WHERE id = ?")
            .bind(state)
            .bind(revision)
            .bind(updated_at)
            .bind(session_id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(reconcile_authoritative_writes(&pool).await.unwrap(), 1);
    }
    let session_states: Vec<(String, i64)> = fetch_pending_deltas(&pool, &pairing_id, 20)
        .await
        .unwrap()
        .iter()
        .filter(|entry| entry.delta_kind == "session-summary")
        .map(|entry| {
            (
                entry.payload["state"].as_str().unwrap().to_string(),
                entry.payload["revision"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        session_states,
        [("running", 0), ("completed", 1), ("running", 2)]
            .map(|(state, revision)| (state.to_string(), revision))
    );
    assert_eq!(reconcile_authoritative_writes(&pool).await.unwrap(), 0);
    assert_eq!(
        fetch_pending_deltas(&pool, &pairing_id, 20)
            .await
            .unwrap()
            .len(),
        7
    );

    let content_pairing =
        seed_publication_pairing(&pool, 501, repository_id, PublicationClass::ContentShared).await;
    sqlx::query(
        "INSERT INTO control_plane_remote_objects \
         (pairing_id, local_kind, local_id, remote_id, class, published_at) \
         VALUES (?, 'repository-consent', ?, ?, 'content-shared:internal', ?)",
    )
    .bind(&content_pairing)
    .bind(repository_id)
    .bind(Uuid::now_v7().to_string())
    .bind(Utc::now().to_rfc3339())
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        reconcile_authoritative_writes_for_pairing(&pool, &content_pairing)
            .await
            .unwrap()
            >= 1
    );
    let content_session = fetch_pending_deltas(&pool, &content_pairing, 20)
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.delta_kind == "session-summary" && entry.subject_id == session_id)
        .unwrap();
    assert_eq!(content_session.class, PublicationClass::ContentShared);
    assert_eq!(content_session.payload["title"], "Repair me");
}

#[tokio::test]
async fn run_sync_revision_preserves_revisited_state_after_ack_and_reconcile_is_idempotent() {
    let (_tmp, pool) = setup_test_db().await;
    let repository_id = "run-cycle-local-repository";
    let pairing_id =
        seed_publication_pairing(&pool, 501, repository_id, PublicationClass::MetadataShared).await;
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO control_plane_remote_objects \
         (pairing_id, local_kind, local_id, remote_id, class, published_at) \
         VALUES (?, 'repository-consent', ?, ?, 'metadata-shared', ?)",
    )
    .bind(&pairing_id)
    .bind(repository_id)
    .bind(Uuid::now_v7().to_string())
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();

    let session_id = "run-cycle-session";
    let run_id = codypendent_protocol::RunId::new();
    let run_id_string = run_id.to_string();
    sqlx::query(
        "INSERT INTO sessions \
         (id, title, state, created_at, updated_at, revision, owner_uid, repository_id) \
         VALUES (?, 'Run cycle', 'open', ?, ?, 0, 501, ?)",
    )
    .bind(session_id)
    .bind(&now)
    .bind(&now)
    .bind(repository_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO runs \
         (id, session_id, objective, state, mode, model_policy, budget_json, started_at) \
         VALUES (?, ?, 'cycle', 'Running', 'Build', 'hosted-default', '{}', ?)",
    )
    .bind(&run_id_string)
    .bind(session_id)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(reconcile_authoritative_writes(&pool).await.unwrap(), 2);
    let running = fetch_pending_deltas(&pool, &pairing_id, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.delta_kind == "run-summary")
        .expect("initial running snapshot");
    acknowledge_receipt(
        &pool,
        &pairing_id,
        running.sequence,
        "receipt-run-running-0",
        Utc::now(),
    )
    .await
    .unwrap();

    projections::set_run_state_with_outbox(&pool, run_id, RunState::Paused)
        .await
        .unwrap();
    let paused = fetch_pending_deltas(&pool, &pairing_id, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.delta_kind == "run-summary")
        .expect("paused snapshot");
    acknowledge_receipt(
        &pool,
        &pairing_id,
        paused.sequence,
        "receipt-run-paused-1",
        Utc::now(),
    )
    .await
    .unwrap();

    projections::set_run_state_with_outbox(&pool, run_id, RunState::Running)
        .await
        .unwrap();

    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT payload, payload_hash, delivery_state FROM control_plane_outbox \
         WHERE pairing_id = ? AND delta_kind = 'run-summary' AND subject_id = ? \
         ORDER BY sequence",
    )
    .bind(&pairing_id)
    .bind(&run_id_string)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter()
            .map(|(payload, _, _)| {
                let payload: serde_json::Value = serde_json::from_str(payload).unwrap();
                (
                    payload["state"].as_str().unwrap().to_string(),
                    payload["sync_revision"].as_i64().unwrap(),
                )
            })
            .collect::<Vec<_>>(),
        vec![
            ("Running".to_string(), 0),
            ("Paused".to_string(), 1),
            ("Running".to_string(), 2),
        ]
    );
    assert_eq!(
        rows.iter()
            .map(|(_, _, delivery_state)| delivery_state.as_str())
            .collect::<Vec<_>>(),
        vec!["acknowledged", "acknowledged", "pending"]
    );
    assert_eq!(
        rows.iter()
            .map(|(_, payload_hash, _)| payload_hash)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );

    assert_eq!(
        reconcile_authoritative_writes_for_pairing(&pool, &pairing_id)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn startup_reconciliation_does_not_scan_authoritative_tables_when_unpaired() {
    let (_tmp, pool) = setup_test_db().await;
    sqlx::query(
        "INSERT INTO artifacts \
         (id, sha256, media_type, byte_length, classification, created_at, provenance_json) \
         VALUES ('malformed', ?, 'text/plain', 1, 'Internal', ?, 'not-json')",
    )
    .bind("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff")
    .bind(Utc::now().to_rfc3339())
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(reconcile_authoritative_writes(&pool).await.unwrap(), 0);
}

#[tokio::test]
async fn startup_reconciliation_fails_closed_on_unknown_remote_classification() {
    let (_tmp, pool) = setup_test_db().await;
    let repository_id = "classification-corrupt-repository";
    let pairing_id =
        seed_publication_pairing(&pool, 501, repository_id, PublicationClass::MetadataShared).await;
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO sessions \
         (id, title, state, created_at, updated_at, revision, owner_uid, repository_id) \
         VALUES ('classification-corrupt-session', 'local', 'open', ?, ?, 0, 501, ?)",
    )
    .bind(&now)
    .bind(&now)
    .bind(repository_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO control_plane_policy_snapshot \
         (pairing_id, policy_version, max_publication_class, max_classification, \
          restrictions, received_at, payload_hash) \
         VALUES (?, 1, 'metadata-shared', 'future-secret', '{}', ?, ?)",
    )
    .bind(&pairing_id)
    .bind(&now)
    .bind("9999999999999999999999999999999999999999999999999999999999999999")
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(reconcile_authoritative_writes(&pool).await.unwrap(), 0);
    assert!(fetch_pending_deltas(&pool, &pairing_id, 20)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn startup_reconciliation_accepts_federated_consent_and_gates_graph_batch_class() {
    let (_tmp, pool) = setup_test_db().await;
    let repository_id = Uuid::now_v7().to_string();
    let federated_id = "1111111111111111111111111111111111111111111111111111111111111111";
    let pairing_id =
        seed_publication_pairing(&pool, 501, federated_id, PublicationClass::ContentShared).await;
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO federated_repository_identity \
         (repository_id, federated_id, root_commit, normalized_remote, display_name, \
          established_at, established_by_uid) \
         VALUES (?, ?, '1234567', 'example.test/acme/repo', 'repo', ?, 501)",
    )
    .bind(&repository_id)
    .bind(federated_id)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO graph_publication_policy \
         (repository_id, max_class, max_classification, policy_version, updated_at, updated_by_uid) \
         VALUES (?, 'content-shared', 'internal', 1, ?, 501)",
    )
    .bind(&repository_id)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();

    for (batch_id, class, subject_id) in [
        (
            "batch-allowed",
            "metadata-shared",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ),
        (
            "batch-too-wide",
            "organization-knowledge",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ),
    ] {
        sqlx::query(
            "INSERT INTO graph_publication_batch \
             (id, repository_id, owner_uid, idempotency_key, policy_version, state, fact_count, \
              batch_hash, sealed_at, created_at) \
             VALUES (?, ?, 501, ?, 1, 'sealed', 1, ?, ?, ?)",
        )
        .bind(batch_id)
        .bind(&repository_id)
        .bind(batch_id)
        .bind("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")
        .bind(&now)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO graph_publication \
             (id, batch_id, subject_kind, subject_id, repository_id, class, classification, \
              decision, policy_version, content_hash, actor_uid, published_at) \
             VALUES (?, ?, 'node', ?, ?, ?, 'internal', 'published', 1, ?, 501, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(batch_id)
        .bind(subject_id)
        .bind(&repository_id)
        .bind(class)
        .bind("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd")
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO graph_publication_batch \
         (id, repository_id, owner_uid, idempotency_key, policy_version, state, fact_count, \
          batch_hash, sealed_at, created_at) \
         VALUES ('batch-partial-tombstone', ?, 501, 'partial-tombstone', 1, 'sealed', 2, ?, ?, ?)",
    )
    .bind(&repository_id)
    .bind("abababababababababababababababababababababababababababababababab")
    .bind(&now)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();
    for subject_id in [
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
    ] {
        sqlx::query(
            "INSERT INTO graph_publication \
             (id, batch_id, subject_kind, subject_id, repository_id, class, classification, \
              decision, policy_version, content_hash, actor_uid, published_at) \
             VALUES (?, 'batch-partial-tombstone', 'node', ?, ?, 'metadata-shared', \
                     'internal', 'published', 1, ?, 501, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(subject_id)
        .bind(&repository_id)
        .bind("cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd")
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO graph_tombstone \
         (id, repository_id, subject_kind, subject_id, reason, published_class, created_at, created_by_uid) \
         VALUES (?, ?, 'node', ?, 'revoked', 'metadata-shared', ?, 501)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(&repository_id)
    .bind("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee")
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();

    enqueue_delta(
        &pool,
        &pairing_id,
        PublicationClass::ContentShared,
        "graph-batch",
        "cross-repository-batch",
        serde_json::json!({
            "batch_id": "cross-repository-batch",
            "repository_id": "different-repository",
            "facts": [{
                "subject_kind": "node",
                "subject_id": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                "class": "metadata-shared",
                "classification": "internal",
                "content_hash": "efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef",
            }],
        }),
        PublicationClass::ContentShared,
    )
    .await
    .unwrap()
    .expect("seed same-subject batch outside tombstone repository");

    assert_eq!(reconcile_authoritative_writes(&pool).await.unwrap(), 3);
    let pending = fetch_pending_deltas(&pool, &pairing_id, 20).await.unwrap();
    assert!(pending
        .iter()
        .any(|entry| entry.delta_kind == "graph-batch" && entry.subject_id == "batch-allowed"));
    assert!(!pending
        .iter()
        .any(|entry| entry.subject_id == "batch-too-wide"));
    assert!(pending.iter().any(|entry| entry.delta_kind == "tombstone"));
    assert!(pending
        .iter()
        .any(|entry| entry.subject_id == "cross-repository-batch"));
    assert!(!pending
        .iter()
        .any(|entry| entry.subject_id == "batch-partial-tombstone"));
    let superseded_graph_batch: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM control_plane_outbox \
         WHERE pairing_id = ? AND subject_id = 'batch-partial-tombstone' \
           AND rejection_code = 'local-tombstone-superseded'",
    )
    .bind(&pairing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(superseded_graph_batch, 1);

    let graph_batch = pending
        .iter()
        .find(|entry| entry.subject_id == "batch-allowed")
        .unwrap();
    acknowledge_receipt(
        &pool,
        &pairing_id,
        graph_batch.sequence,
        "receipt-batch",
        Utc::now(),
    )
    .await
    .unwrap();
    let batch_state: (String, Option<String>) = sqlx::query_as(
        "SELECT state, remote_receipt FROM graph_publication_batch WHERE id = 'batch-allowed'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        batch_state,
        (
            "acknowledged".to_string(),
            Some("receipt-batch".to_string())
        )
    );

    let tombstone = pending
        .iter()
        .find(|entry| entry.delta_kind == "tombstone")
        .unwrap();
    acknowledge_receipt(
        &pool,
        &pairing_id,
        tombstone.sequence,
        "receipt-tombstone-1",
        Utc::now(),
    )
    .await
    .unwrap();
    let acknowledged: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM graph_tombstone WHERE acknowledged_at IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(acknowledged, 1);
    let superseded_native_batch: (String, Option<String>) = sqlx::query_as(
        "SELECT state, remote_receipt FROM graph_publication_batch \
         WHERE id = 'batch-partial-tombstone'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        superseded_native_batch,
        ("sealed".to_string(), None),
        "retiring a pending outbox occurrence must not falsely acknowledge the immutable native batch"
    );

    let pairing_b =
        seed_publication_pairing(&pool, 501, federated_id, PublicationClass::ContentShared).await;
    sqlx::query(
        "INSERT INTO control_plane_remote_objects \
         (pairing_id, local_kind, local_id, remote_id, class, published_at) \
         VALUES (?, 'repository-consent', ?, ?, 'metadata-shared', ?)",
    )
    .bind(&pairing_b)
    .bind(&repository_id)
    .bind(Uuid::now_v7().to_string())
    .bind(Utc::now().to_rfc3339())
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        reconcile_authoritative_writes_for_pairing(&pool, &pairing_b)
            .await
            .unwrap()
            >= 1
    );
    assert!(fetch_pending_deltas(&pool, &pairing_b, 20)
        .await
        .unwrap()
        .iter()
        .any(|entry| entry.delta_kind == "tombstone"
            && entry.payload["native_tombstone_id"] == tombstone.payload["native_tombstone_id"]));

    let second_tombstone_id = Uuid::now_v7().to_string();
    let second_created_at = (Utc::now() + chrono::Duration::seconds(1)).to_rfc3339();
    sqlx::query(
        "INSERT INTO graph_tombstone \
         (id, repository_id, subject_kind, subject_id, reason, published_class, created_at, created_by_uid) \
         VALUES (?, ?, 'node', ?, 'revoked', 'metadata-shared', ?, 501)",
    )
    .bind(&second_tombstone_id)
    .bind(&repository_id)
    .bind("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee")
    .bind(&second_created_at)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(reconcile_authoritative_writes(&pool).await.unwrap(), 2);
    let pending = fetch_pending_deltas(&pool, &pairing_id, 20).await.unwrap();
    let second_tombstone = pending
        .iter()
        .find(|entry| {
            entry.delta_kind == "tombstone"
                && entry.payload["native_tombstone_id"] == second_tombstone_id
        })
        .unwrap();
    acknowledge_receipt(
        &pool,
        &pairing_id,
        second_tombstone.sequence,
        "receipt-tombstone-2",
        Utc::now(),
    )
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO graph_publication_batch \
         (id, repository_id, owner_uid, idempotency_key, policy_version, state, fact_count, created_at) \
         VALUES ('batch-after-tombstone', ?, 501, 'after-tombstone', 1, 'building', 0, ?)",
    )
    .bind(&repository_id)
    .bind(Utc::now().to_rfc3339())
    .execute(&pool)
    .await
    .unwrap();
    let store = codypendent_federation::SharedGraphStore::new(pool.clone());
    let sealed = store.seal_batch("batch-after-tombstone").await.unwrap();
    assert_eq!(sealed.state.as_str(), "sealed");
}

#[tokio::test]
async fn inbound_receipt_and_stream_cursor_idempotency() {
    let (_tmp, pool) = setup_test_db().await;
    let pairing_id = Uuid::now_v7().to_string();

    let pairing = ControlPlanePairing {
        id: pairing_id.clone(),
        owner_uid: 501,
        endpoint: "https://control-plane.test".to_string(),
        organization_id: "org_test".to_string(),
        organization_display_name: "Test Org".to_string(),
        consent_manifest: "{}".to_string(),
        consent_manifest_hash: "1111111111111111111111111111111111111111111111111111111111111111"
            .to_string(),
        max_publication_class: PublicationClass::MetadataShared,
        accepts_remote_approvals: false,
        accepts_runner_dispatch: false,
        state: PairingState::Active,
        paired_at: Some(Utc::now()),
        expires_at: None,
        revoked_at: None,
        revoked_reason: None,
        created_at: Utc::now(),
    };
    let cred = ControlPlaneCredential {
        pairing_id: pairing_id.clone(),
        credential_ref: "keychain:test".to_string(),
        credential_hash: "2222222222222222222222222222222222222222222222222222222222222222"
            .to_string(),
        audience: "https://control-plane.test".to_string(),
        purpose: "sync".to_string(),
        issued_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::days(1),
        rotated_at: None,
    };
    record_pairing(&pool, &pairing, &cred).await.unwrap();

    let remote_msg_id = "msg_456";
    assert!(!has_inbound_receipt(&pool, &pairing_id, remote_msg_id)
        .await
        .unwrap());

    let receipt = InboundReceipt {
        pairing_id: pairing_id.clone(),
        remote_message_id: remote_msg_id.to_string(),
        message_kind: "approval".to_string(),
        local_effect_id: Some("approval_effect_1".to_string()),
        outcome_hash: "3333333333333333333333333333333333333333333333333333333333333333"
            .to_string(),
        received_at: Utc::now(),
    };

    record_inbound_receipt(&pool, &receipt).await.unwrap();
    assert!(has_inbound_receipt(&pool, &pairing_id, remote_msg_id)
        .await
        .unwrap());

    // Test stream cursor
    assert!(get_stream_cursor(&pool, &pairing_id, "approvals")
        .await
        .unwrap()
        .is_none());
    set_stream_cursor(&pool, &pairing_id, "approvals", "42")
        .await
        .unwrap();
    assert_eq!(
        get_stream_cursor(&pool, &pairing_id, "approvals")
            .await
            .unwrap()
            .as_deref(),
        Some("42")
    );
}

#[tokio::test]
async fn policy_snapshot_and_effective_narrowing() {
    let (_tmp, pool) = setup_test_db().await;
    let pairing_id = Uuid::now_v7().to_string();

    let pairing = ControlPlanePairing {
        id: pairing_id.clone(),
        owner_uid: 501,
        endpoint: "https://control-plane.test".to_string(),
        organization_id: "org_test".to_string(),
        organization_display_name: "Test Org".to_string(),
        consent_manifest: "{}".to_string(),
        consent_manifest_hash: "1111111111111111111111111111111111111111111111111111111111111111"
            .to_string(),
        max_publication_class: PublicationClass::ContentShared,
        accepts_remote_approvals: false,
        accepts_runner_dispatch: false,
        state: PairingState::Active,
        paired_at: Some(Utc::now()),
        expires_at: None,
        revoked_at: None,
        revoked_reason: None,
        created_at: Utc::now(),
    };
    let cred = ControlPlaneCredential {
        pairing_id: pairing_id.clone(),
        credential_ref: "keychain:test".to_string(),
        credential_hash: "2222222222222222222222222222222222222222222222222222222222222222"
            .to_string(),
        audience: "https://control-plane.test".to_string(),
        purpose: "sync".to_string(),
        issued_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::days(1),
        rotated_at: None,
    };
    record_pairing(&pool, &pairing, &cred).await.unwrap();

    let mut restrictions = PolicyRestrictions::default();
    restrictions
        .denied_providers
        .push("untrusted-provider".to_string());
    restrictions
        .denied_models
        .push("experimental-999".to_string());
    restrictions.denied_regions.push("eu-west-9".to_string());
    restrictions
        .denied_integrations
        .push("unvetted-plugin".to_string());

    let snapshot = PolicySnapshot {
        policy_version: 5,
        max_publication_class: PublicationClass::MetadataShared, // Narrower than local ContentShared
        max_classification: DataClassification::Internal,
        restrictions,
        received_at: Utc::now(),
        payload_hash: Sha256Digest(
            "4444444444444444444444444444444444444444444444444444444444444444".to_string(),
        ),
    };

    store_policy_snapshot(&pool, &pairing_id, &snapshot)
        .await
        .unwrap();

    let effective = compute_effective_policy(
        &pool,
        &pairing_id,
        PublicationClass::ContentShared,
        DataClassification::Confidential,
    )
    .await
    .unwrap();

    // Strictest (narrowest) wins: MetadataShared < ContentShared
    assert_eq!(
        effective.publication_class,
        PublicationClass::MetadataShared
    );
    // Strictest classification wins: Internal < Confidential
    assert_eq!(effective.classification, DataClassification::Internal);

    // Check restrictions
    assert!(!effective.is_provider_allowed("untrusted-provider"));
    assert!(effective.is_provider_allowed("openai"));
    assert!(!effective.is_model_allowed("experimental-999"));
    assert!(effective.is_model_allowed("claude-3-5-sonnet"));
    assert!(!effective.is_region_allowed("eu-west-9"));
    assert!(effective.is_region_allowed("us-east-1"));
    assert!(!effective.is_integration_allowed("unvetted-plugin"));
    assert!(effective.is_integration_allowed("github"));
}

#[tokio::test]
async fn policy_snapshot_store_is_monotonic_idempotent_and_integrity_checked() {
    let (_tmp, pool) = setup_test_db().await;
    let pairing_id = seed_publication_pairing(
        &pool,
        501,
        "policy-integrity-repository",
        PublicationClass::ContentShared,
    )
    .await;
    let snapshot = |policy_version, max_publication_class, max_classification, hash_byte: char| {
        PolicySnapshot {
            policy_version,
            max_publication_class,
            max_classification,
            restrictions: PolicyRestrictions {
                denied_providers: vec![format!("denied-at-{policy_version}")],
                ..PolicyRestrictions::default()
            },
            received_at: Utc::now(),
            payload_hash: Sha256Digest(hash_byte.to_string().repeat(64)),
        }
    };

    let version_two = snapshot(
        2,
        PublicationClass::MetadataShared,
        DataClassification::Internal,
        '2',
    );
    store_policy_snapshot(&pool, &pairing_id, &version_two)
        .await
        .unwrap();
    store_policy_snapshot(&pool, &pairing_id, &version_two)
        .await
        .expect("an exact replay is idempotent");

    let stale = snapshot(
        1,
        PublicationClass::ContentShared,
        DataClassification::Confidential,
        '1',
    );
    store_policy_snapshot(&pool, &pairing_id, &stale)
        .await
        .expect("a stale delivery is a successful no-op so its cursor may advance");
    set_stream_cursor(&pool, &pairing_id, "policy", "cursor-after-stale")
        .await
        .unwrap();
    assert_eq!(
        get_stream_cursor(&pool, &pairing_id, "policy")
            .await
            .unwrap()
            .as_deref(),
        Some("cursor-after-stale")
    );
    let stored = get_policy_snapshot(&pool, &pairing_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.policy_version, 2);
    assert_eq!(stored.payload_hash, "2".repeat(64));

    let conflicting_equal = snapshot(
        2,
        PublicationClass::PrivateLocal,
        DataClassification::Public,
        'a',
    );
    let error = store_policy_snapshot(&pool, &pairing_id, &conflicting_equal)
        .await
        .expect_err("the same version cannot name a different authoritative payload");
    assert!(matches!(
        error,
        ControlPlaneSyncError::PolicyViolation(reason) if reason.contains("conflicts")
    ));
    assert_eq!(
        get_policy_snapshot(&pool, &pairing_id)
            .await
            .unwrap()
            .unwrap()
            .payload_hash,
        "2".repeat(64),
        "an integrity conflict does not mutate the stored authority"
    );

    let version_three = snapshot(
        3,
        PublicationClass::PrivateLocal,
        DataClassification::Public,
        '3',
    );
    store_policy_snapshot(&pool, &pairing_id, &version_three)
        .await
        .expect("a higher version replaces the prior snapshot");
    let stored = get_policy_snapshot(&pool, &pairing_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.policy_version, 3);
    assert_eq!(stored.max_publication_class, PublicationClass::PrivateLocal);
    assert_eq!(stored.max_classification, DataClassification::Public);
    assert_eq!(stored.payload_hash, "3".repeat(64));

    for invalid in [
        snapshot(
            4,
            PublicationClass::Unknown,
            DataClassification::Internal,
            '4',
        ),
        snapshot(
            4,
            PublicationClass::MetadataShared,
            DataClassification::Unknown,
            '5',
        ),
        snapshot(
            u64::MAX,
            PublicationClass::MetadataShared,
            DataClassification::Internal,
            '6',
        ),
    ] {
        assert!(matches!(
            store_policy_snapshot(&pool, &pairing_id, &invalid).await,
            Err(ControlPlaneSyncError::PolicyViolation(_))
        ));
    }
    assert_eq!(
        get_policy_snapshot(&pool, &pairing_id)
            .await
            .unwrap()
            .unwrap()
            .policy_version,
        3,
        "invalid authority never reaches storage"
    );

    sqlx::query(
        "UPDATE control_plane_policy_snapshot SET max_classification = 'future-class' \
         WHERE pairing_id = ?",
    )
    .bind(&pairing_id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        compute_effective_policy(
            &pool,
            &pairing_id,
            PublicationClass::ContentShared,
            DataClassification::Confidential,
        )
        .await,
        Err(ControlPlaneSyncError::PolicyViolation(_))
    ));
}

#[tokio::test]
async fn sync_engine_offline_and_mock_server_sync() {
    let (_tmp, pool) = setup_test_db().await;

    // 1. Unpaired test: sync_all_active_once does zero work
    let engine = SyncEngine::new(pool.clone());
    let synced_count = engine.sync_all_active_once().await.unwrap();
    assert_eq!(
        synced_count, 0,
        "Unpaired daemon must perform 0 sync operations"
    );

    // 2. Setup mock control plane server
    let pushed_deltas_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let pushed_counter = pushed_deltas_count.clone();
    let policy_pulls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let policy_pull_counter = policy_pulls.clone();
    let daemon_id = Uuid::now_v7();
    let organization_id = Uuid::now_v7();
    let repository_id = Uuid::now_v7();
    let federated_id = "e".repeat(64);
    let repository_catalog = catalog_repository(organization_id, repository_id, &federated_id);

    let mock_app = Router::new()
        .route(
            "/v1/sync/push",
            post(move |Json(req): Json<SyncDeltaPushRequest>| {
                let counter = pushed_counter.clone();
                async move {
                    assert!(req.deltas.iter().all(|delta| {
                        delta.repository_id.map(|id| id.as_uuid()) == Some(repository_id)
                    }));
                    assert!(req.deltas.iter().all(|delta| {
                        delta
                            .payload
                            .get("repository_id")
                            .and_then(|value| value.as_str())
                            == Some("repo_abc")
                    }));
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let latest_sequence = req
                        .deltas
                        .iter()
                        .map(|delta| delta.sequence)
                        .max()
                        .unwrap_or(0);
                    let receipts = req
                        .deltas
                        .into_iter()
                        .map(|delta| codypendent_control_plane_protocol::SyncReceipt {
                            id: codypendent_control_plane_protocol::SyncReceiptId::new(),
                            daemon_id: req.daemon_id,
                            daemon_sequence: delta.sequence,
                            delta_kind: delta.kind,
                            payload_hash: delta.payload_hash,
                            class: delta.class,
                            accepted_at: Utc::now(),
                            duplicate: false,
                        })
                        .collect();
                    Json(SyncDeltaPushResponse {
                        receipts,
                        latest_sequence,
                        rejected_deltas: Vec::new(),
                    })
                }
            }),
        )
        .route(
            "/v1/sync/pull",
            get(
                move |Query(params): Query<std::collections::HashMap<String, String>>| {
                    let policy_pull_counter = policy_pull_counter.clone();
                    async move {
                        let stream = params.get("stream").map(|s| s.as_str()).unwrap_or("sync");
                        if stream == "policy" {
                            policy_pull_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            assert!(!params.contains_key("repository_id"));
                            if params
                                .get("after_id")
                                .and_then(|value| value.parse::<i64>().ok())
                                .unwrap_or(0)
                                >= 1
                            {
                                return Json(vec![]);
                            }
                            let event = StreamEvent {
                                id: 1,
                                organization_id: codypendent_control_plane_protocol::OrganizationId(
                                    Uuid::now_v7(),
                                ),
                                repository_id: None,
                                stream: StreamKind::Policy,
                                payload: StreamEventPayload::PolicyUpdate(PolicyUpdateEvent {
                                    policy_version: 2,
                                    max_publication_class: PublicationClass::MetadataShared,
                                    max_classification: DataClassification::Internal,
                                }),
                                created_at: Utc::now(),
                            };
                            Json(vec![event])
                        } else {
                            assert_eq!(
                                params
                                    .get("repository_id")
                                    .and_then(|value| Uuid::parse_str(value).ok()),
                                Some(repository_id)
                            );
                            Json(vec![])
                        }
                    }
                },
            ),
        )
        .route(
            "/v1/organizations/{organization_id}/repositories",
            get(move |headers: HeaderMap| {
                let repository = repository_catalog.clone();
                async move {
                    assert_eq!(
                        headers
                            .get(AUTHORIZATION)
                            .and_then(|value| value.to_str().ok()),
                        Some("Bearer mock_token_123")
                    );
                    Json(vec![repository])
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, mock_app).await.unwrap();
    });

    let endpoint = format!("http://{}", addr);

    // 3. Register pairing with mock server endpoint
    let pairing_id = daemon_id.to_string();
    let manifest = LocalConsentManifest {
        organization_id: organization_id.to_string(),
        organization_display_name: "Mock Org".to_string(),
        endpoint: endpoint.clone(),
        max_publication_class: PublicationClass::MetadataShared,
        accepts_remote_approvals: false,
        accepts_runner_dispatch: false,
        allowed_repositories: vec![repository_id.to_string()],
        created_at: Utc::now(),
    };
    let pairing = ControlPlanePairing {
        id: pairing_id.clone(),
        owner_uid: 501,
        endpoint: endpoint.clone(),
        organization_id: organization_id.to_string(),
        organization_display_name: "Mock Org".to_string(),
        consent_manifest: serde_json::to_string(&manifest).unwrap(),
        consent_manifest_hash: manifest.compute_hash(),
        max_publication_class: PublicationClass::MetadataShared,
        accepts_remote_approvals: false,
        accepts_runner_dispatch: false,
        state: PairingState::Active,
        paired_at: Some(Utc::now()),
        expires_at: None,
        revoked_at: None,
        revoked_reason: None,
        created_at: Utc::now(),
    };
    let cred = ControlPlaneCredential {
        pairing_id: pairing_id.clone(),
        credential_ref: "keychain:mock".to_string(),
        credential_hash: Sha256Digest::from_bytes(b"mock_token_123").0,
        audience: "control-plane".to_string(),
        purpose: "sync".to_string(),
        issued_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::days(1),
        rotated_at: None,
    };
    record_pairing(&pool, &pairing, &cred).await.unwrap();
    engine
        .set_pairing_token(&pairing_id, "mock_token_123")
        .await;

    sqlx::query(
        "INSERT INTO federated_repository_identity \
         (repository_id, federated_id, root_commit, normalized_remote, display_name, \
          established_at, established_by_uid) \
         VALUES ('repo_abc', ?, '1234567', NULL, 'Mock repository', ?, 501)",
    )
    .bind(&federated_id)
    .bind(Utc::now().to_rfc3339())
    .execute(&pool)
    .await
    .unwrap();

    // These authoritative rows predate the first catalog resolution. An
    // interim control-plane UUID manifest must recover them once the local
    // alias is linked through its federated identity.
    let authoritative_session_id = "sess_preexisting_uuid_consent";
    let authoritative_run_id = codypendent_protocol::RunId::new();
    let authoritative_now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO sessions \
         (id, title, state, created_at, updated_at, revision, owner_uid, repository_id) \
         VALUES (?, 'Preexisting', 'open', ?, ?, 0, 501, 'repo_abc')",
    )
    .bind(authoritative_session_id)
    .bind(&authoritative_now)
    .bind(&authoritative_now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO runs \
         (id, session_id, objective, state, mode, model_policy, budget_json, started_at) \
         VALUES (?, ?, 'preexisting', 'Running', 'Build', 'hosted-default', '{}', ?)",
    )
    .bind(authoritative_run_id.to_string())
    .bind(authoritative_session_id)
    .bind(&authoritative_now)
    .execute(&pool)
    .await
    .unwrap();

    // Enqueue two outbox items
    let repository_key = "repo_abc".to_string();
    enqueue_session_summary(
        &pool,
        &pairing_id,
        PublicationClass::MetadataShared,
        "sess_mock_1",
        Some(&repository_key),
        "active",
        Utc::now(),
        None,
        None,
        PublicationClass::MetadataShared,
    )
    .await
    .unwrap();

    enqueue_run_summary(
        &pool,
        &pairing_id,
        PublicationClass::MetadataShared,
        "run_mock_1",
        "sess_mock_1",
        Some(&repository_key),
        "running",
        Utc::now(),
        None,
        None,
        PublicationClass::MetadataShared,
    )
    .await
    .unwrap();

    // 4. Run sync cycle
    let summary = engine.sync_pairing_once(&pairing_id).await.unwrap();
    assert_eq!(summary.pushed_deltas, 4);
    assert_eq!(summary.acknowledged_deltas, 4);
    assert_eq!(summary.pulled_events, 1);
    assert_eq!(
        pushed_deltas_count.load(std::sync::atomic::Ordering::SeqCst),
        1
    );

    // Verify outbox is now empty of pending items
    let pending_after = fetch_pending_deltas(&pool, &pairing_id, 10).await.unwrap();
    assert_eq!(pending_after.len(), 0);

    // An unchanged catalog mapping must not trigger a repository-history scan
    // on every poll. This raw write intentionally bypasses the production
    // enqueue path; startup reconciliation remains responsible for repairing
    // such crash-window rows.
    let bypassed_now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO sessions \
         (id, title, state, created_at, updated_at, revision, owner_uid, repository_id) \
         VALUES ('sess_not_rescanned', 'Bypassed', 'open', ?, ?, 0, 501, 'repo_abc')",
    )
    .bind(&bypassed_now)
    .bind(&bypassed_now)
    .execute(&pool)
    .await
    .unwrap();

    // A production artifact write after the first resolution must use the
    // cached pairing-scoped mapping to target this UUID-consent pairing.
    let store = ArtifactStore::new(_tmp.path().join("uuid-consent-artifacts"));
    let artifact = store
        .put(
            &pool,
            "text/plain",
            ArtifactClassification::Internal,
            Provenance::tool_output("shell.run", authoritative_run_id),
            b"publication after UUID consent resolution",
        )
        .await
        .unwrap();
    let production_pending = fetch_pending_deltas(&pool, &pairing_id, 10).await.unwrap();
    assert_eq!(production_pending.len(), 1);
    assert_eq!(production_pending[0].delta_kind, "artifact-summary");
    assert_eq!(production_pending[0].subject_id, artifact.id.to_string());

    let production_summary = engine.sync_pairing_once(&pairing_id).await.unwrap();
    assert_eq!(production_summary.pushed_deltas, 1);
    assert_eq!(production_summary.acknowledged_deltas, 1);
    assert_eq!(
        pushed_deltas_count.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert_eq!(
        policy_pulls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "policy is pulled exactly once per pairing cycle"
    );
    assert_eq!(
        get_repository_stream_cursor(&pool, &pairing_id, "", "policy")
            .await
            .unwrap()
            .as_deref(),
        Some("1")
    );
    assert!(
        get_repository_stream_cursor(&pool, &pairing_id, &repository_id.to_string(), "policy")
            .await
            .unwrap()
            .is_none()
    );

    // Verify inbound policy event was applied
    let policy_snapshot =
        codypendent_daemon::control_plane_sync::get_policy_snapshot(&pool, &pairing_id)
            .await
            .unwrap()
            .expect("policy snapshot stored from inbound stream");
    assert_eq!(policy_snapshot.policy_version, 2);

    server_handle.abort();
}

/// More permanent blockers than one outbound batch used to recur forever at
/// the head of the ordered queue, so no later delta could ever be submitted.
/// Permanent per-delta refusals must be durable dead letters, while a policy
/// refusal remains pending for a future grant or ceiling change.
#[tokio::test]
async fn permanent_rejections_leave_the_head_batch_without_consuming_transient_retries() {
    let (_tmp, pool) = setup_test_db().await;

    let pushed_batches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let accepted_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let push_batches = pushed_batches.clone();
    let accepted_counter = accepted_seen.clone();
    let daemon_id = Uuid::now_v7();
    let organization_id = Uuid::now_v7();
    let repository_id = Uuid::now_v7();
    let federated_id = "f".repeat(64);
    let pairing_id = daemon_id.to_string();
    let repository_key = federated_id.clone();
    let expected_outbound_identity = federated_id.clone();
    let repository_catalog = catalog_repository(organization_id, repository_id, &federated_id);
    let mock_app = Router::new()
        .route(
            "/v1/sync/push",
            post(move |Json(req): Json<SyncDeltaPushRequest>| {
                let push_batches = push_batches.clone();
                let accepted_counter = accepted_counter.clone();
                let expected_outbound_identity = expected_outbound_identity.clone();
                async move {
                    assert!(req.deltas.iter().all(|delta| {
                        delta.repository_id.map(|id| id.as_uuid()) == Some(repository_id)
                    }));
                    assert!(req.deltas.iter().all(|delta| {
                        delta
                            .payload
                            .get("repository_id")
                            .and_then(|value| value.as_str())
                            == Some(expected_outbound_identity.as_str())
                    }));
                    push_batches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let daemon_id = req.daemon_id;
                    let latest_sequence = req
                        .deltas
                        .iter()
                        .map(|delta| delta.sequence)
                        .max()
                        .unwrap_or(0);
                    let mut receipts = Vec::new();
                    let mut rejected_deltas = Vec::new();
                    for delta in req.deltas {
                        if delta.subject_id.starts_with("blocker-") {
                            rejected_deltas.push(SyncRejection {
                                sequence: delta.sequence,
                                code: "malformed-delta".to_string(),
                                reason: "payload cannot be accepted".to_string(),
                            });
                        } else if delta.subject_id == "transient" {
                            rejected_deltas.push(SyncRejection {
                                sequence: delta.sequence,
                                code: "delta-refused".to_string(),
                                reason: "repository grant is not currently available".to_string(),
                            });
                        } else {
                            accepted_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            receipts.push(codypendent_control_plane_protocol::SyncReceipt {
                                id: codypendent_control_plane_protocol::SyncReceiptId::new(),
                                daemon_id,
                                daemon_sequence: delta.sequence,
                                delta_kind: delta.kind,
                                payload_hash: delta.payload_hash,
                                class: delta.class,
                                accepted_at: Utc::now(),
                                duplicate: false,
                            });
                        }
                    }
                    Json(SyncDeltaPushResponse {
                        receipts,
                        latest_sequence,
                        rejected_deltas,
                    })
                }
            }),
        )
        .route(
            "/v1/sync/pull",
            get(|| async { Json(Vec::<StreamEvent>::new()) }),
        )
        .route(
            "/v1/organizations/{organization_id}/repositories",
            get(move |headers: HeaderMap| {
                let repository = repository_catalog.clone();
                async move {
                    assert_eq!(
                        headers
                            .get(AUTHORIZATION)
                            .and_then(|value| value.to_str().ok()),
                        Some("Bearer dead_letter_token")
                    );
                    Json(vec![repository])
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, mock_app).await.unwrap();
    });

    let manifest = LocalConsentManifest {
        organization_id: organization_id.to_string(),
        organization_display_name: "Dead-letter test".to_string(),
        endpoint: endpoint.clone(),
        max_publication_class: PublicationClass::MetadataShared,
        accepts_remote_approvals: false,
        accepts_runner_dispatch: false,
        allowed_repositories: vec![federated_id.clone()],
        created_at: Utc::now(),
    };
    let pairing = ControlPlanePairing {
        id: pairing_id.clone(),
        owner_uid: 501,
        endpoint: endpoint.clone(),
        organization_id: organization_id.to_string(),
        organization_display_name: "Dead-letter test".to_string(),
        consent_manifest: serde_json::to_string(&manifest).unwrap(),
        consent_manifest_hash: manifest.compute_hash(),
        max_publication_class: PublicationClass::MetadataShared,
        accepts_remote_approvals: false,
        accepts_runner_dispatch: false,
        state: PairingState::Active,
        paired_at: Some(Utc::now()),
        expires_at: None,
        revoked_at: None,
        revoked_reason: None,
        created_at: Utc::now(),
    };
    let token = "dead_letter_token";
    let credential = ControlPlaneCredential {
        pairing_id: pairing_id.clone(),
        credential_ref: "keychain:dead-letter".to_string(),
        credential_hash: Sha256Digest::from_bytes(token.as_bytes()).0,
        audience: "control-plane".to_string(),
        purpose: "sync".to_string(),
        issued_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::days(1),
        rotated_at: None,
    };
    record_pairing(&pool, &pairing, &credential).await.unwrap();

    let engine = SyncEngine::new(pool.clone());
    engine.set_pairing_token(&pairing_id, token).await;

    // Fifty-one permanent blockers span more than DEFAULT_BATCH_SIZE. The
    // accepted and transient rows are deliberately behind all of them.
    for index in 0..51 {
        let subject_id = format!("blocker-{index}");
        enqueue_session_summary(
            &pool,
            &pairing_id,
            PublicationClass::MetadataShared,
            &subject_id,
            Some(&repository_key),
            "completed",
            Utc::now(),
            None,
            None,
            PublicationClass::MetadataShared,
        )
        .await
        .unwrap()
        .expect("unique blocker enqueued");
    }
    for subject_id in ["later-accepted", "transient"] {
        enqueue_session_summary(
            &pool,
            &pairing_id,
            PublicationClass::MetadataShared,
            subject_id,
            Some(&repository_key),
            "completed",
            Utc::now(),
            None,
            None,
            PublicationClass::MetadataShared,
        )
        .await
        .unwrap()
        .expect("tail delta enqueued");
    }

    let first = engine.sync_pairing_once(&pairing_id).await.unwrap();
    assert_eq!(first.pushed_deltas, 50);
    assert_eq!(first.acknowledged_deltas, 0);
    assert_eq!(first.failed_deltas, 50);

    let second = engine.sync_pairing_once(&pairing_id).await.unwrap();
    assert_eq!(second.pushed_deltas, 3);
    assert_eq!(second.acknowledged_deltas, 1);
    assert_eq!(second.failed_deltas, 2);
    assert_eq!(
        accepted_seen.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a later delta must reach the server after permanent blockers retire"
    );

    // The transient refusal remains the sole pending row and is retried. None
    // of the 51 permanent blockers reappear in this third batch.
    let third = engine.sync_pairing_once(&pairing_id).await.unwrap();
    assert_eq!(third.pushed_deltas, 1);
    assert_eq!(third.acknowledged_deltas, 0);
    assert_eq!(third.failed_deltas, 1);
    assert_eq!(pushed_batches.load(std::sync::atomic::Ordering::SeqCst), 3);

    let rejected_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM control_plane_outbox WHERE pairing_id = ? AND delivery_state = 'rejected'",
    )
    .bind(&pairing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rejected_count, 51);

    let (code, reason, rejected_at): (Option<String>, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT rejection_code, rejection_reason, rejected_at FROM control_plane_outbox WHERE pairing_id = ? AND subject_id = 'blocker-0'",
        )
        .bind(&pairing_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(code.as_deref(), Some("malformed-delta"));
    assert_eq!(reason.as_deref(), Some("payload cannot be accepted"));
    assert!(rejected_at.is_some());

    let (state, attempts, transient_code): (String, i64, Option<String>) = sqlx::query_as(
        "SELECT delivery_state, attempts, rejection_code FROM control_plane_outbox WHERE pairing_id = ? AND subject_id = 'transient'",
    )
    .bind(&pairing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "pending");
    assert_eq!(attempts, 2);
    assert!(transient_code.is_none());

    server_handle.abort();
}

#[tokio::test]
async fn local_corruption_is_dead_lettered_without_starving_a_later_valid_delta() {
    let (_tmp, pool) = setup_test_db().await;
    let daemon_id = Uuid::now_v7();
    let organization_id = Uuid::now_v7();
    let repository_id = Uuid::now_v7();
    let federated_id = "9".repeat(64);
    let pairing_id = daemon_id.to_string();
    let catalog = catalog_repository(organization_id, repository_id, &federated_id);
    let pushed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let pushed_counter = pushed.clone();

    let app = Router::new()
        .route(
            "/v1/organizations/{organization_id}/repositories",
            get(move |headers: HeaderMap| {
                let repository = catalog.clone();
                async move {
                    assert_eq!(
                        headers
                            .get(AUTHORIZATION)
                            .and_then(|value| value.to_str().ok()),
                        Some("Bearer local-corruption-token")
                    );
                    Json(vec![repository])
                }
            }),
        )
        .route(
            "/v1/sync/push",
            post(move |Json(request): Json<SyncDeltaPushRequest>| {
                let pushed_counter = pushed_counter.clone();
                async move {
                    pushed_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    assert_eq!(request.deltas.len(), 1);
                    let delta = &request.deltas[0];
                    assert_eq!(delta.subject_id, "valid-after-corruption");
                    assert_eq!(
                        delta.repository_id.map(|id| id.as_uuid()),
                        Some(repository_id)
                    );
                    Json(SyncDeltaPushResponse {
                        receipts: vec![codypendent_control_plane_protocol::SyncReceipt {
                            id: codypendent_control_plane_protocol::SyncReceiptId::new(),
                            daemon_id: request.daemon_id,
                            daemon_sequence: delta.sequence,
                            delta_kind: delta.kind,
                            payload_hash: delta.payload_hash.clone(),
                            class: delta.class,
                            accepted_at: Utc::now(),
                            duplicate: false,
                        }],
                        latest_sequence: delta.sequence,
                        rejected_deltas: Vec::new(),
                    })
                }
            }),
        )
        .route(
            "/v1/sync/pull",
            get(|| async { Json(Vec::<StreamEvent>::new()) }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let manifest = LocalConsentManifest {
        organization_id: organization_id.to_string(),
        organization_display_name: "Local corruption".to_string(),
        endpoint: endpoint.clone(),
        max_publication_class: PublicationClass::MetadataShared,
        accepts_remote_approvals: false,
        accepts_runner_dispatch: false,
        allowed_repositories: vec![federated_id.clone()],
        created_at: Utc::now(),
    };
    let pairing = ControlPlanePairing {
        id: pairing_id.clone(),
        owner_uid: 501,
        endpoint: endpoint.clone(),
        organization_id: organization_id.to_string(),
        organization_display_name: manifest.organization_display_name.clone(),
        consent_manifest: serde_json::to_string(&manifest).unwrap(),
        consent_manifest_hash: manifest.compute_hash(),
        max_publication_class: PublicationClass::MetadataShared,
        accepts_remote_approvals: false,
        accepts_runner_dispatch: false,
        state: PairingState::Active,
        paired_at: Some(Utc::now()),
        expires_at: None,
        revoked_at: None,
        revoked_reason: None,
        created_at: Utc::now(),
    };
    let token = "local-corruption-token";
    let credential = ControlPlaneCredential {
        pairing_id: pairing_id.clone(),
        credential_ref: "keychain:local-corruption".to_string(),
        credential_hash: Sha256Digest::from_bytes(token.as_bytes()).0,
        audience: "control-plane".to_string(),
        purpose: "sync".to_string(),
        issued_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::days(1),
        rotated_at: None,
    };
    record_pairing(&pool, &pairing, &credential).await.unwrap();

    // A full head page plus one more deterministic identity failure exercises
    // the pagination path before the later valid row becomes visible.
    for index in 0..51 {
        enqueue_session_summary(
            &pool,
            &pairing_id,
            PublicationClass::MetadataShared,
            &format!("invalid-identity-{index}"),
            Some("repo_unconsented"),
            "completed",
            Utc::now(),
            None,
            None,
            PublicationClass::MetadataShared,
        )
        .await
        .unwrap()
        .expect("enqueue invalid identity fixture");
    }

    let next_sequence: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM control_plane_outbox WHERE pairing_id = ?",
    )
    .bind(&pairing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    for (offset, subject_id, payload, class) in [
        (0_i64, "malformed-json", "not-json", "metadata-shared"),
        (
            1_i64,
            "unknown-class",
            "{\"repository_id\":\"9999999999999999999999999999999999999999999999999999999999999999\"}",
            "future-class",
        ),
        (
            2_i64,
            "private-class",
            "{\"repository_id\":\"9999999999999999999999999999999999999999999999999999999999999999\"}",
            "private-local",
        ),
    ] {
        sqlx::query(
            "INSERT INTO control_plane_outbox \
             (id, pairing_id, delta_kind, subject_id, payload, class, payload_hash, sequence, created_at) \
             VALUES (?, ?, 'session-summary', ?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&pairing_id)
        .bind(subject_id)
        .bind(payload)
        .bind(class)
        .bind("7".repeat(64))
        .bind(next_sequence + offset)
        .bind(Utc::now().to_rfc3339())
        .execute(&pool)
        .await
        .unwrap();
    }
    enqueue_session_summary(
        &pool,
        &pairing_id,
        PublicationClass::MetadataShared,
        "valid-after-corruption",
        Some(&federated_id),
        "completed",
        Utc::now(),
        None,
        None,
        PublicationClass::MetadataShared,
    )
    .await
    .unwrap()
    .expect("enqueue later valid row");

    let engine = SyncEngine::new(pool.clone());
    engine.set_pairing_token(&pairing_id, token).await;
    let summary = engine.sync_pairing_once(&pairing_id).await.unwrap();

    assert_eq!(summary.failed_deltas, 54);
    assert_eq!(summary.pushed_deltas, 1);
    assert_eq!(summary.acknowledged_deltas, 1);
    assert_eq!(pushed.load(std::sync::atomic::Ordering::SeqCst), 1);
    let locally_rejected: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM control_plane_outbox \
         WHERE pairing_id = ? AND delivery_state = 'rejected' \
           AND rejection_code = 'local-invalid-delta'",
    )
    .bind(&pairing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(locally_rejected, 54);
    let malformed_reason: String = sqlx::query_scalar(
        "SELECT rejection_reason FROM control_plane_outbox \
         WHERE pairing_id = ? AND subject_id = 'malformed-json'",
    )
    .bind(&pairing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(malformed_reason, "outbox payload is not valid JSON");
    assert!(fetch_pending_deltas(&pool, &pairing_id, 10)
        .await
        .unwrap()
        .is_empty());

    server.abort();
}

#[tokio::test]
async fn authenticated_repository_ceilings_are_enforced_before_the_first_push() {
    let (_tmp, pool) = setup_test_db().await;
    let daemon_id = Uuid::now_v7();
    let organization_id = Uuid::now_v7();
    let repository_id = Uuid::now_v7();
    let federated_id = "8".repeat(64);
    let pairing_id = daemon_id.to_string();
    let catalog = catalog_repository(organization_id, repository_id, &federated_id);
    let widened = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let catalog_widened = widened.clone();
    let policy_widened = widened.clone();
    let pushed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let pushed_counter = pushed.clone();

    let app = Router::new()
        .route(
            "/v1/organizations/{organization_id}/repositories",
            get(move || {
                let mut repository = catalog.clone();
                let is_wide = catalog_widened.load(std::sync::atomic::Ordering::SeqCst);
                async move {
                    if is_wide {
                        repository.max_publication_class = PublicationClass::ContentShared;
                        repository.max_classification = DataClassification::Internal;
                        repository.policy_version = 2;
                    } else {
                        repository.max_publication_class = PublicationClass::ContentShared;
                        repository.max_classification = DataClassification::Public;
                    }
                    Json(vec![repository])
                }
            }),
        )
        .route(
            "/v1/sync/push",
            post(move |Json(request): Json<SyncDeltaPushRequest>| {
                let pushed_counter = pushed_counter.clone();
                async move {
                    let call = pushed_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let wire = serde_json::to_string(&request).unwrap();
                    assert!(!wire.contains("_local_supersedes_outbox_id"));
                    if call == 0 {
                        assert_eq!(request.deltas.len(), 1);
                        let delta = &request.deltas[0];
                        assert_eq!(
                            delta.kind,
                            codypendent_control_plane_protocol::SyncDeltaKind::SessionSummary
                        );
                        assert_eq!(delta.subject_id, "repair-wide-session");
                        assert_eq!(delta.class, PublicationClass::MetadataShared);
                        assert!(delta.payload["title"].is_null());
                        assert!(!wire.contains("SECRET TITLE"));
                        assert!(!wire.contains("SECRET ARTIFACT BYTES"));
                        assert!(!wire.contains("SECRET GRAPH BYTES"));
                    } else {
                        assert_eq!(call, 1, "policy widening must publish each blocked row once");
                        assert_eq!(request.deltas.len(), 2);
                        assert!(request.deltas.iter().any(|delta| {
                            delta.kind
                                == codypendent_control_plane_protocol::SyncDeltaKind::ArtifactSummary
                                && delta.payload["content"] == "SECRET ARTIFACT BYTES"
                        }));
                        assert!(request.deltas.iter().any(|delta| {
                            delta.kind
                                == codypendent_control_plane_protocol::SyncDeltaKind::GraphBatch
                                && delta.payload["facts"][0]["source"] == "SECRET GRAPH BYTES"
                        }));
                    }
                    let latest_sequence = request
                        .deltas
                        .iter()
                        .map(|delta| delta.sequence)
                        .max()
                        .unwrap_or(0);
                    Json(SyncDeltaPushResponse {
                        receipts: request
                            .deltas
                            .iter()
                            .map(|delta| codypendent_control_plane_protocol::SyncReceipt {
                                id: codypendent_control_plane_protocol::SyncReceiptId::new(),
                                daemon_id: request.daemon_id,
                                daemon_sequence: delta.sequence,
                                delta_kind: delta.kind,
                                payload_hash: delta.payload_hash.clone(),
                                class: delta.class,
                                accepted_at: Utc::now(),
                                duplicate: false,
                            })
                            .collect(),
                        latest_sequence,
                        rejected_deltas: Vec::new(),
                    })
                }
            }),
        )
        .route(
            "/v1/sync/pull",
            get(move |Query(params): Query<std::collections::HashMap<String, String>>| {
                let is_wide = policy_widened.load(std::sync::atomic::Ordering::SeqCst);
                async move {
                    if params.get("stream").map(String::as_str) != Some("policy") {
                        return Json(Vec::<StreamEvent>::new());
                    }
                    assert!(!params.contains_key("repository_id"));
                    let after_id = params
                        .get("after_id")
                        .and_then(|value| value.parse::<i64>().ok())
                        .unwrap_or(0);
                    let events = if is_wide {
                        if after_id >= 52 {
                            Vec::new()
                        } else {
                            vec![(
                                52_u64,
                                PublicationClass::ContentShared,
                                DataClassification::Internal,
                            )]
                        }
                    } else if after_id == 0 {
                        (1_u64..=50)
                            .map(|id| {
                                (
                                    id,
                                    PublicationClass::ContentShared,
                                    DataClassification::Internal,
                                )
                            })
                            .collect()
                    } else if after_id == 50 {
                        vec![(
                            51_u64,
                            PublicationClass::MetadataShared,
                            DataClassification::Public,
                        )]
                    } else {
                        Vec::new()
                    };
                    Json(
                        events
                            .into_iter()
                            .map(
                                |(id, max_publication_class, max_classification)| StreamEvent {
                                    id,
                                    organization_id: OrganizationId::from_uuid(organization_id),
                                    repository_id: None,
                                    stream: StreamKind::Policy,
                                    payload: StreamEventPayload::PolicyUpdate(PolicyUpdateEvent {
                                        policy_version: id,
                                        max_publication_class,
                                        max_classification,
                                    }),
                                    created_at: Utc::now(),
                                },
                            )
                            .collect(),
                    )
                }
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let manifest = LocalConsentManifest {
        organization_id: organization_id.to_string(),
        organization_display_name: "Repository ceiling".to_string(),
        endpoint: endpoint.clone(),
        max_publication_class: PublicationClass::ContentShared,
        accepts_remote_approvals: false,
        accepts_runner_dispatch: false,
        allowed_repositories: vec![federated_id.clone()],
        created_at: Utc::now(),
    };
    let pairing = ControlPlanePairing {
        id: pairing_id.clone(),
        owner_uid: 501,
        endpoint,
        organization_id: organization_id.to_string(),
        organization_display_name: manifest.organization_display_name.clone(),
        consent_manifest: serde_json::to_string(&manifest).unwrap(),
        consent_manifest_hash: manifest.compute_hash(),
        max_publication_class: PublicationClass::ContentShared,
        accepts_remote_approvals: false,
        accepts_runner_dispatch: false,
        state: PairingState::Active,
        paired_at: Some(Utc::now()),
        expires_at: None,
        revoked_at: None,
        revoked_reason: None,
        created_at: Utc::now(),
    };
    let token = "repository-ceiling-token";
    let credential = ControlPlaneCredential {
        pairing_id: pairing_id.clone(),
        credential_ref: "keychain:repository-ceiling".to_string(),
        credential_hash: Sha256Digest::from_bytes(token.as_bytes()).0,
        audience: "control-plane".to_string(),
        purpose: "sync".to_string(),
        issued_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::days(1),
        rotated_at: None,
    };
    record_pairing(&pool, &pairing, &credential).await.unwrap();

    let session_outbox_id = enqueue_session_summary(
        &pool,
        &pairing_id,
        PublicationClass::ContentShared,
        "too-wide-session",
        Some(&federated_id),
        "active",
        Utc::now(),
        None,
        Some("SECRET TITLE"),
        PublicationClass::ContentShared,
    )
    .await
    .unwrap()
    .expect("enqueue content session");
    let newer_session_id = enqueue_session_summary(
        &pool,
        &pairing_id,
        PublicationClass::ContentShared,
        "too-wide-session",
        Some(&federated_id),
        "completed",
        Utc::now(),
        None,
        None,
        PublicationClass::MetadataShared,
    )
    .await
    .unwrap()
    .expect("enqueue newer metadata session");
    let newer_session = fetch_pending_deltas(&pool, &pairing_id, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.id == newer_session_id)
        .unwrap();
    acknowledge_receipt(
        &pool,
        &pairing_id,
        newer_session.sequence,
        "receipt-newer-session",
        Utc::now(),
    )
    .await
    .unwrap();
    let repair_session_outbox_id = enqueue_session_summary(
        &pool,
        &pairing_id,
        PublicationClass::ContentShared,
        "repair-wide-session",
        Some(&federated_id),
        "active",
        Utc::now(),
        None,
        Some("SECRET TITLE"),
        PublicationClass::ContentShared,
    )
    .await
    .unwrap()
    .expect("enqueue ambiguous wide session needing a fresh repair sequence");
    enqueue_delta(
        &pool,
        &pairing_id,
        PublicationClass::ContentShared,
        "artifact-summary",
        "classified-artifact",
        serde_json::json!({
            "artifact_id": "classified-artifact",
            "repository_id": federated_id,
            "classification": "internal",
            "content": "SECRET ARTIFACT BYTES",
        }),
        PublicationClass::ContentShared,
    )
    .await
    .unwrap()
    .expect("enqueue classified artifact");
    enqueue_delta(
        &pool,
        &pairing_id,
        PublicationClass::ContentShared,
        "graph-batch",
        "classified-graph",
        serde_json::json!({
            "batch_id": "classified-graph",
            "repository_id": federated_id,
            "facts": [{
                "subject_kind": "node",
                "subject_id": "node-secret",
                "class": "content-shared",
                "classification": "internal",
                "source": "SECRET GRAPH BYTES",
            }],
        }),
        PublicationClass::ContentShared,
    )
    .await
    .unwrap()
    .expect("enqueue classified graph");

    let engine = SyncEngine::new(pool.clone());
    engine.set_pairing_token(&pairing_id, token).await;
    let summary = engine.sync_pairing_once(&pairing_id).await.unwrap();

    assert_eq!(summary.failed_deltas, 2);
    assert_eq!(summary.pushed_deltas, 1);
    assert_eq!(summary.acknowledged_deltas, 1);
    assert_eq!(pushed.load(std::sync::atomic::Ordering::SeqCst), 1);
    let original: (String, String, String) = sqlx::query_as(
        "SELECT delivery_state, class, payload FROM control_plane_outbox WHERE id = ?",
    )
    .bind(&session_outbox_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(original.0, "rejected");
    assert_eq!(original.1, "content-shared");
    let narrowed: (String, String, String) = sqlx::query_as(
        "SELECT delivery_state, class, payload FROM control_plane_outbox \
         WHERE pairing_id = ? AND delta_kind = 'session-summary' \
           AND subject_id = 'too-wide-session' AND id <> ?",
    )
    .bind(&pairing_id)
    .bind(&session_outbox_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(narrowed.0, "acknowledged");
    assert_eq!(narrowed.1, "metadata-shared");
    assert!(!narrowed.2.contains("SECRET TITLE"));
    assert!(!narrowed.2.contains("_local_supersedes_outbox_id"));
    let repair_rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT delivery_state, payload, sequence FROM control_plane_outbox \
         WHERE pairing_id = ? AND delta_kind = 'session-summary' \
           AND subject_id = 'repair-wide-session' ORDER BY sequence ASC",
    )
    .bind(&pairing_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(repair_rows.len(), 2);
    assert_eq!(repair_rows[0].0, "rejected");
    assert_eq!(repair_rows[1].0, "acknowledged");
    assert!(repair_rows[1].2 > repair_rows[0].2);
    assert!(!repair_rows[1].1.contains("SECRET TITLE"));
    assert_eq!(repair_rows[0].2, {
        sqlx::query_scalar::<_, i64>("SELECT sequence FROM control_plane_outbox WHERE id = ?")
            .bind(&repair_session_outbox_id)
            .fetch_one(&pool)
            .await
            .unwrap()
    });
    let blocked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM control_plane_outbox \
         WHERE pairing_id = ? AND rejection_code = 'local-policy-blocked'",
    )
    .bind(&pairing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(blocked, 2);
    assert!(fetch_pending_deltas(&pool, &pairing_id, 10)
        .await
        .unwrap()
        .is_empty());

    widened.store(true, std::sync::atomic::Ordering::SeqCst);
    let widened_summary = engine.sync_pairing_once(&pairing_id).await.unwrap();
    assert_eq!(widened_summary.failed_deltas, 0);
    assert_eq!(widened_summary.pushed_deltas, 2);
    assert_eq!(widened_summary.acknowledged_deltas, 2);
    assert_eq!(pushed.load(std::sync::atomic::Ordering::SeqCst), 2);

    let final_summary = engine.sync_pairing_once(&pairing_id).await.unwrap();
    assert_eq!(final_summary.pushed_deltas, 0);
    assert_eq!(pushed.load(std::sync::atomic::Ordering::SeqCst), 2);
    let still_blocked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM control_plane_outbox \
         WHERE pairing_id = ? AND rejection_code = 'local-policy-blocked'",
    )
    .bind(&pairing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(still_blocked, 0);

    server.abort();
}
