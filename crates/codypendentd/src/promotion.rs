//! The daemon's promotion-pipeline host (Phase 7 STEP 7.5).
//!
//! Like [`WorkflowConductorHost`](crate::workflows::WorkflowConductorHost),
//! this lives in the assembly binary because it bridges the daemon (which
//! declares the [`PromotionGateway`] seam) and `codypendent-eval` (which owns
//! the [`Candidate`] state machine and the durable [`PromotionStore`]). The
//! daemon crate cannot name the eval crate, so the composition happens here.
//!
//! [`PromotionStoreGateway`] fills the seam by delegating every method
//! straight to [`PromotionStore`] over the daemon's pool — there is no
//! additional logic to get wrong here, which is deliberate: the state-machine
//! rules (no self-promotion, no unobserved canary) live in `codypendent-eval`
//! and this host must not re-implement (or worse, loosen) them. Its only two
//! jobs are (1) translating the wire-carried artifact `kind` string into an
//! [`ArtifactKind`] and (2) attributing a fresh proposal's author.

use codypendent_daemon::promotion::{
    AdvancePromotionRequest, ApprovePromotionRequest, PromotionActionFuture, PromotionGateway,
    PromotionProposeFuture, ProposePromotionRequest, RollbackPromotionRequest,
};
use codypendent_eval::{
    ArtifactKind, ArtifactVersion, CanaryOutcome, PromotionStore, PromotionStoreError,
};
use codypendent_protocol::{Actor, CodypendentError, PromotionAction};
use sqlx::SqlitePool;

/// Drives the promotion pipeline over the daemon's pool. Cheap to clone (a
/// pool handle plus a stateless store), matching
/// [`WorkflowConductorHost`](crate::workflows::WorkflowConductorHost)'s style.
#[derive(Clone)]
pub struct PromotionStoreGateway {
    pool: SqlitePool,
    store: PromotionStore,
}

impl PromotionStoreGateway {
    /// Build a gateway over the daemon's pool. The promotion tables share the
    /// daemon's pool (the migrations are workspace-wide).
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            store: PromotionStore::new(),
        }
    }
}

impl PromotionGateway for PromotionStoreGateway {
    fn propose(&self, request: ProposePromotionRequest) -> PromotionProposeFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            let kind = ArtifactKind::parse(&request.kind).ok_or_else(|| {
                CodypendentError::new(
                    "promotion.invalid-kind",
                    format!("unrecognized artifact kind {:?}", request.kind),
                    false,
                )
            })?;
            let artifact = ArtifactVersion::new(kind, request.name, request.version);
            // A CLI/socket-submitted proposal is attributed to the submitting
            // CLIENT, not claimed as human or agent — an agent-synthesized
            // proposal (from a future grader/clustering pipeline, not wired by
            // this task) would attribute `Actor::Agent` instead; either way,
            // authorship never implies approval (only `ApprovePromotion`'s
            // `Actor::Human` mapping does that).
            let author = Actor::Client {
                client_id: request.client_id,
            };
            host.store
                .propose_idempotent(
                    &host.pool,
                    &request.idempotency_key,
                    artifact,
                    &author,
                    request.requires_permission_review,
                )
                .await
                .map_err(store_error_to_protocol)
        })
    }

    fn advance(&self, request: AdvancePromotionRequest) -> PromotionActionFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            match request.action {
                PromotionAction::RunRegression { regressed } => host
                    .store
                    .run_regression(&host.pool, &request.candidate_id, regressed)
                    .await
                    .map_err(store_error_to_protocol),
                PromotionAction::StartShadow => host
                    .store
                    .start_shadow(&host.pool, &request.candidate_id)
                    .await
                    .map_err(store_error_to_protocol),
                PromotionAction::StartCanary => host
                    .store
                    .start_canary(&host.pool, &request.candidate_id)
                    .await
                    .map_err(store_error_to_protocol),
                PromotionAction::ObserveCanary { regressed } => host
                    .store
                    .observe_canary(&host.pool, &request.candidate_id, regressed)
                    .await
                    .map(|outcome| {
                        // The auto-rollback record is already persisted (with
                        // its own audit row) by the store; the command reply
                        // only needs to signal success, so the outcome itself
                        // is not surfaced further here (a `PromotionProposed`-
                        // style reply carrying it is a natural follow-up if a
                        // client ever needs to react to an auto-rollback
                        // synchronously — see the report's deferred-items list).
                        let _: CanaryOutcome = outcome;
                    })
                    .map_err(store_error_to_protocol),
                PromotionAction::FinishCanary => host
                    .store
                    .finish_canary(&host.pool, &request.candidate_id)
                    .await
                    .map_err(store_error_to_protocol),
                // `PromotionAction::Unknown` and any future, `#[non_exhaustive]`
                // variant this build does not know (RULE 1) — reject rather
                // than guess at a transition.
                _ => Err(CodypendentError::new(
                    "promotion.unknown-action",
                    "unrecognized promotion action".to_string(),
                    false,
                )),
            }
        })
    }

    fn approve(&self, request: ApprovePromotionRequest) -> PromotionActionFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            host.store
                .approve(&host.pool, &request.candidate_id, &request.approver)
                .await
                .map(|_record| ())
                .map_err(store_error_to_protocol)
        })
    }

    fn rollback(&self, request: RollbackPromotionRequest) -> PromotionActionFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            host.store
                .rollback(&host.pool, &request.candidate_id, &request.actor)
                .await
                .map(|_record| ())
                .map_err(store_error_to_protocol)
        })
    }
}

/// Map a [`PromotionStoreError`] to the wire [`CodypendentError`] a client
/// branches on by code. A store/database hiccup is retryable; every semantic
/// rejection (unknown candidate, illegal transition, non-human approver,
/// unobserved canary, permission review still pending) is not — retrying an
/// unchanged request would fail identically.
fn store_error_to_protocol(error: PromotionStoreError) -> CodypendentError {
    let message = error.to_string();
    let code = match &error {
        PromotionStoreError::NotFound(_) => "promotion.not-found",
        PromotionStoreError::Corrupt(_) => "promotion.corrupt",
        PromotionStoreError::Promotion(inner) => promotion_error_code(inner),
        PromotionStoreError::Database(_) | PromotionStoreError::Serde(_) => "promotion.store-error",
    };
    let retryable = matches!(
        error,
        PromotionStoreError::Database(_) | PromotionStoreError::Serde(_)
    );
    CodypendentError::new(code, message, retryable)
}

fn promotion_error_code(error: &codypendent_eval::PromotionError) -> &'static str {
    use codypendent_eval::PromotionError;
    match error {
        PromotionError::RequiresHumanApproval { .. } => "promotion.requires-human-approval",
        PromotionError::RegressedOffline => "promotion.regressed-offline",
        PromotionError::IllegalTransition { .. } => "promotion.illegal-transition",
        PromotionError::PermissionReviewRequired => "promotion.permission-review-required",
        PromotionError::NotPromoted { .. } => "promotion.not-promoted",
        PromotionError::CanaryUnobserved => "promotion.canary-unobserved",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_daemon::promotion::{
        AdvancePromotionRequest, ApprovePromotionRequest, ProposePromotionRequest,
        RollbackPromotionRequest,
    };
    use codypendent_eval::PromotionStage;
    use codypendent_protocol::ids::{AgentId, ModelId, RunId, UserId};
    use codypendent_protocol::ClientId;

    async fn temp_pool() -> (tempfile::TempDir, SqlitePool) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = codypendent_eval::db::open(&tmp.path().join("codypendent.db"))
            .await
            .unwrap();
        (tmp, pool)
    }

    fn human_client_id() -> ClientId {
        ClientId::new()
    }

    /// Mirrors exactly how `crates/daemon/src/server.rs` maps a `Controller`
    /// connection to `Actor::Human` for `ApprovePromotion`/`RollbackPromotion`
    /// — the daemon's own construction is exercised by the daemon-crate's
    /// `server_it.rs` role-gating tests; this test exercises what the gateway
    /// does once handed that actor.
    fn human_actor(client_id: ClientId) -> Actor {
        Actor::Human {
            user_id: UserId(client_id.to_string()),
        }
    }

    fn agent_actor() -> Actor {
        Actor::Agent {
            agent_id: AgentId::new(),
            run_id: RunId::new(),
            model: ModelId("claude-sonnet-5".into()),
        }
    }

    #[tokio::test]
    async fn a_controller_mapped_human_drives_a_candidate_to_promoted_and_active() {
        let (_tmp, pool) = temp_pool().await;
        let gateway = PromotionStoreGateway::new(pool.clone());
        let client_id = human_client_id();

        let candidate_id = gateway
            .propose(ProposePromotionRequest {
                kind: "router".to_string(),
                name: "tool-selection".to_string(),
                version: 4,
                requires_permission_review: false,
                idempotency_key: "propose-1".to_string(),
                client_id,
            })
            .await
            .expect("propose accepted");

        for action in [
            PromotionAction::RunRegression { regressed: false },
            PromotionAction::StartShadow,
            PromotionAction::StartCanary,
            PromotionAction::ObserveCanary { regressed: false },
            PromotionAction::FinishCanary,
        ] {
            gateway
                .advance(AdvancePromotionRequest {
                    candidate_id: candidate_id.clone(),
                    action,
                    client_id,
                })
                .await
                .expect("advance accepted");
        }

        gateway
            .approve(ApprovePromotionRequest {
                candidate_id: candidate_id.clone(),
                approver: human_actor(client_id),
                client_id,
            })
            .await
            .expect("a Controller-mapped human approval succeeds");

        let snapshot = PromotionStore::new()
            .get(&pool, &candidate_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.candidate.stage(), PromotionStage::Promoted);
        assert_eq!(
            PromotionStore::new()
                .active_version(&pool, "router/tool-selection")
                .await
                .unwrap(),
            Some(4),
            "approval activates the version"
        );
    }

    #[tokio::test]
    async fn an_agent_actor_handed_to_approve_is_refused_even_by_the_real_gateway() {
        // The gateway performs NO actor gating itself (see the module doc — it
        // must not re-implement or loosen the rule); this proves the guard it
        // relies on (`Candidate::approve`) still holds when reached through
        // the full assembly, not just the bare eval-crate type. The daemon's
        // OWN role gate (server.rs) is what actually prevents an
        // `Actor::Agent` from ever being constructed for this command in
        // production; see `crates/daemon/tests/server_it.rs`.
        let (_tmp, pool) = temp_pool().await;
        let gateway = PromotionStoreGateway::new(pool.clone());
        let client_id = human_client_id();

        let candidate_id = gateway
            .propose(ProposePromotionRequest {
                kind: "skill".to_string(),
                name: "rust-ci".to_string(),
                version: 1,
                requires_permission_review: false,
                idempotency_key: "propose-2".to_string(),
                client_id,
            })
            .await
            .unwrap();
        for action in [
            PromotionAction::RunRegression { regressed: false },
            PromotionAction::StartShadow,
            PromotionAction::StartCanary,
            PromotionAction::ObserveCanary { regressed: false },
            PromotionAction::FinishCanary,
        ] {
            gateway
                .advance(AdvancePromotionRequest {
                    candidate_id: candidate_id.clone(),
                    action,
                    client_id,
                })
                .await
                .unwrap();
        }

        let error = gateway
            .approve(ApprovePromotionRequest {
                candidate_id: candidate_id.clone(),
                approver: agent_actor(),
                client_id,
            })
            .await
            .expect_err("an agent actor must never reach Promoted");
        assert_eq!(error.code, "promotion.requires-human-approval");

        let snapshot = PromotionStore::new()
            .get(&pool, &candidate_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.candidate.stage(), PromotionStage::ComparisonReady);
    }

    #[tokio::test]
    async fn finishing_an_unobserved_canary_is_rejected_with_the_right_code() {
        let (_tmp, pool) = temp_pool().await;
        let gateway = PromotionStoreGateway::new(pool);
        let client_id = human_client_id();
        let candidate_id = gateway
            .propose(ProposePromotionRequest {
                kind: "prompt".to_string(),
                name: "coding-agent".to_string(),
                version: 2,
                requires_permission_review: false,
                idempotency_key: "propose-3".to_string(),
                client_id,
            })
            .await
            .unwrap();
        for action in [
            PromotionAction::RunRegression { regressed: false },
            PromotionAction::StartShadow,
            PromotionAction::StartCanary,
        ] {
            gateway
                .advance(AdvancePromotionRequest {
                    candidate_id: candidate_id.clone(),
                    action,
                    client_id,
                })
                .await
                .unwrap();
        }

        let error = gateway
            .advance(AdvancePromotionRequest {
                candidate_id: candidate_id.clone(),
                action: PromotionAction::FinishCanary,
                client_id,
            })
            .await
            .expect_err("zero observations must not finish the canary");
        assert_eq!(error.code, "promotion.canary-unobserved");
    }

    #[tokio::test]
    async fn a_canary_regression_auto_rolls_back_and_manual_rollback_is_attributed() {
        let (_tmp, pool) = temp_pool().await;
        let gateway = PromotionStoreGateway::new(pool.clone());
        let client_id = human_client_id();
        let candidate_id = gateway
            .propose(ProposePromotionRequest {
                kind: "router".to_string(),
                name: "escalation".to_string(),
                version: 1,
                requires_permission_review: false,
                idempotency_key: "propose-4".to_string(),
                client_id,
            })
            .await
            .unwrap();
        for action in [
            PromotionAction::RunRegression { regressed: false },
            PromotionAction::StartShadow,
            PromotionAction::StartCanary,
        ] {
            gateway
                .advance(AdvancePromotionRequest {
                    candidate_id: candidate_id.clone(),
                    action,
                    client_id,
                })
                .await
                .unwrap();
        }
        // A regression signal auto-rolls-back — no approve/rollback command
        // needed, and the audit trail attributes "system" (proven at the
        // store layer already; here it just must not surface as an error).
        gateway
            .advance(AdvancePromotionRequest {
                candidate_id: candidate_id.clone(),
                action: PromotionAction::ObserveCanary { regressed: true },
                client_id,
            })
            .await
            .expect("an auto-rollback is a successful advance, not an error");
        let snapshot = PromotionStore::new()
            .get(&pool, &candidate_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.candidate.stage(), PromotionStage::RolledBack);

        // A second, unrelated candidate promoted then manually rolled back —
        // the manual path attributes the mapped human actor.
        let promoted_id = gateway
            .propose(ProposePromotionRequest {
                kind: "router".to_string(),
                name: "escalation".to_string(),
                version: 2,
                requires_permission_review: false,
                idempotency_key: "propose-5".to_string(),
                client_id,
            })
            .await
            .unwrap();
        for action in [
            PromotionAction::RunRegression { regressed: false },
            PromotionAction::StartShadow,
            PromotionAction::StartCanary,
            PromotionAction::ObserveCanary { regressed: false },
            PromotionAction::FinishCanary,
        ] {
            gateway
                .advance(AdvancePromotionRequest {
                    candidate_id: promoted_id.clone(),
                    action,
                    client_id,
                })
                .await
                .unwrap();
        }
        gateway
            .approve(ApprovePromotionRequest {
                candidate_id: promoted_id.clone(),
                approver: human_actor(client_id),
                client_id,
            })
            .await
            .unwrap();
        gateway
            .rollback(RollbackPromotionRequest {
                candidate_id: promoted_id.clone(),
                actor: human_actor(client_id),
                client_id,
            })
            .await
            .expect("manual rollback of a promoted candidate succeeds");
        let snapshot = PromotionStore::new()
            .get(&pool, &promoted_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.candidate.stage(), PromotionStage::RolledBack);
    }

    #[tokio::test]
    async fn unrecognized_artifact_kind_is_rejected_before_touching_the_store() {
        let (_tmp, pool) = temp_pool().await;
        let gateway = PromotionStoreGateway::new(pool);
        let error = gateway
            .propose(ProposePromotionRequest {
                kind: "quantum-flux-capacitor".to_string(),
                name: "n/a".to_string(),
                version: 1,
                requires_permission_review: false,
                idempotency_key: "propose-bad-kind".to_string(),
                client_id: human_client_id(),
            })
            .await
            .expect_err("an unrecognized kind must not silently coerce to some default");
        assert_eq!(error.code, "promotion.invalid-kind");
    }
}
