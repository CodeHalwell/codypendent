//! Golden vectors for the federation command / payload families.
//!
//! `crates/protocol/tests/golden_vectors.rs` covers the daemon protocol, but it
//! stops short of Milestone 6: not one of the fourteen federation
//! `CommandBody` variants, thirteen federation `Payload` variants, or the
//! thirty-odd view types in `crates/protocol/src/federated_graph.rs` had a
//! committed vector. That is the family where a silent wire change is most
//! expensive, because `PublicationClass` decides how far a fact travels off the
//! machine: renaming or reordering a class is a data-egress bug, not a
//! serialization bug.
//!
//! This file lives beside the daemon one and follows its pattern exactly (fixed
//! sentinels, one committed JSON file per family, keys sorted, pretty-printed),
//! but writes into `<repo-root>/protocol-vectors/federation/` so that every
//! pre-existing vector file's committed bytes stay untouched.
//!
//! ## Regenerating
//!
//! ```text
//! cargo test -p codypendent-protocol --test federation_vectors regenerate_vectors -- --ignored
//! ```
//!
//! CI never runs the regenerator (it is `#[ignore]`d and it WRITES files); CI
//! runs [`committed_vectors_match_current_protocol_types`],
//! [`committed_vectors_round_trip_through_their_rust_types`], and the partition
//! guards [`every_federated_graph_type_has_a_golden_vector`] and
//! [`every_federation_command_and_payload_variant_has_a_golden_vector`] — the
//! last of which reads `command.rs` and `envelope.rs` at test time, so a new
//! federation variant added without a vector fails instead of quietly widening
//! the uncovered surface.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use codypendent_protocol::{
    BlastRadiusNode, BlastRadiusQuery, BlastRadiusReport, CampaignApprovalMode,
    CampaignApprovalView, CampaignDetailView, CampaignEffectView, CampaignKind,
    CampaignRepoEnrollment, CampaignRepoState, CampaignRepositoryView, CampaignRunView,
    CampaignState, CampaignView, CommandBody, CreateCampaignRequest, DataClassification,
    ExecuteCampaignRequest, FederatedGraphPage, FederatedGraphQuery,
    FederatedRepositoryIdentityView, GraphPublicationPolicyView, MigrationPlanQuery,
    MigrationPlanReport, MigrationPlanStep, PageCursor, Payload, PublicationBatchSummary,
    PublicationClass, ReviewerSuggestion, ReviewerSuggestionQuery, ReviewerSuggestions,
    SharedEdgeView, SharedNodeView, UpdatePublicationPolicyRequest,
};
use codypendent_protocol::{CommandId, RunId};

// ---------------------------------------------------------------------------
// Sentinels: fixed, readable, non-random.
// ---------------------------------------------------------------------------

fn command_id() -> CommandId {
    "40000000-0000-0000-0000-000000000001".parse().unwrap()
}

fn run_id() -> RunId {
    "30000000-0000-0000-0000-000000000001".parse().unwrap()
}

fn sentinel_time() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .expect("sentinel timestamp parses")
        .with_timezone(&Utc)
}

fn sentinel_time_later() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-01-02T00:00:00Z")
        .expect("sentinel timestamp parses")
        .with_timezone(&Utc)
}

/// The federated id of a repository is a SHA-256 hex digest, never a local
/// path — a fixed one keeps the vectors stable and keeps a real path out of a
/// committed file.
const FEDERATED_ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const OTHER_FEDERATED_ID: &str = "2222222222222222222222222222222222222222222222222222222222222222";

fn page_cursor() -> PageCursor {
    PageCursor("opaque-federation-cursor".to_string())
}

// ---------------------------------------------------------------------------
// Vector / manifest plumbing (mirrors crates/protocol/tests/golden_vectors.rs).
// ---------------------------------------------------------------------------

struct Vector {
    name: &'static str,
    value: Value,
    round_trip: fn(&Value) -> Value,
}

fn vec_of<T>(name: &'static str, instance: T) -> Vector
where
    T: Serialize + DeserializeOwned,
{
    let value = serde_json::to_value(&instance)
        .unwrap_or_else(|e| panic!("{name}: failed to serialize: {e}"));
    Vector {
        name,
        value,
        round_trip: |v: &Value| {
            let parsed: T = serde_json::from_value(v.clone())
                .unwrap_or_else(|e| panic!("failed to deserialize: {e}"));
            serde_json::to_value(&parsed).expect("re-serialize")
        },
    }
}

fn manifest_value(vectors: &[Vector]) -> Value {
    let mut map = serde_json::Map::new();
    for v in vectors {
        let previous = map.insert(v.name.to_string(), v.value.clone());
        assert!(
            previous.is_none(),
            "duplicate vector name {:?} — every vector name must be unique within its file",
            v.name
        );
    }
    Value::Object(map)
}

/// Recursively sort object keys so the rendered bytes are identical whether
/// `serde_json`'s `Map` is `BTreeMap`- or `IndexMap`-backed (a workspace
/// `--all-features` build turns on `preserve_order`). Array order is preserved.
fn sort_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<&String, &Value> = map.iter().collect();
            let object: serde_json::Map<String, Value> = sorted
                .into_iter()
                .map(|(k, v)| (k.clone(), sort_keys(v)))
                .collect();
            Value::Object(object)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_keys).collect()),
        other => other.clone(),
    }
}

fn render(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(&sort_keys(value)).expect("pretty-print vectors");
    text.push('\n');
    text
}

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("protocol-vectors")
}

fn source_file(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(name)
}

// ---------------------------------------------------------------------------
// federated_graph.rs — the view and query types.
// ---------------------------------------------------------------------------

fn shared_node_view() -> SharedNodeView {
    SharedNodeView {
        shared_node_id: "node-1".to_string(),
        repository_id: FEDERATED_ID.to_string(),
        kind: "function".to_string(),
        language: "rust".to_string(),
        package: Some("payments".to_string()),
        qualified_name: Some("payments::charge".to_string()),
        source_path: Some("src/charge.rs".to_string()),
        signature_hash: Some("abc123".to_string()),
        class: PublicationClass::ContentShared,
        classification: DataClassification::Internal,
        revision: "rev-1".to_string(),
    }
}

fn shared_node_view_metadata_only() -> SharedNodeView {
    // At `metadata-shared` the symbol name, source path and signature hash are
    // REDACTED to absent. Not empty strings: an empty string would still say
    // "this node has a name", which is the fact the class exists to withhold.
    SharedNodeView {
        shared_node_id: "node-2".to_string(),
        repository_id: OTHER_FEDERATED_ID.to_string(),
        kind: "function".to_string(),
        language: "rust".to_string(),
        package: None,
        qualified_name: None,
        source_path: None,
        signature_hash: None,
        class: PublicationClass::MetadataShared,
        classification: DataClassification::Internal,
        revision: "rev-1".to_string(),
    }
}

fn shared_edge_view() -> SharedEdgeView {
    SharedEdgeView {
        shared_edge_id: "edge-1".to_string(),
        from_shared_node_id: "node-1".to_string(),
        to_shared_node_id: "node-2".to_string(),
        from_repository_id: FEDERATED_ID.to_string(),
        to_repository_id: OTHER_FEDERATED_ID.to_string(),
        relation: "calls".to_string(),
        confidence: 0.75,
        evidence_kind: "static-analysis".to_string(),
        evidence_artifact: Some("artifact-1".to_string()),
        class: PublicationClass::ContentShared,
        classification: DataClassification::Internal,
        revision: "rev-1".to_string(),
    }
}

fn federated_graph_page() -> FederatedGraphPage {
    FederatedGraphPage {
        nodes: vec![shared_node_view(), shared_node_view_metadata_only()],
        edges: vec![shared_edge_view()],
        cursor: Some(page_cursor()),
        has_more: true,
    }
}

fn federated_identity_view() -> FederatedRepositoryIdentityView {
    FederatedRepositoryIdentityView {
        repository_id: "/home/user/project".to_string(),
        federated_id: FEDERATED_ID.to_string(),
        root_commit: "a1b2c3d4e5f6".to_string(),
        normalized_remote: Some("github.com/octocat/hello-world".to_string()),
        display_name: "hello-world".to_string(),
        established_at: sentinel_time(),
    }
}

fn publication_policy_view() -> GraphPublicationPolicyView {
    GraphPublicationPolicyView {
        repository_id: FEDERATED_ID.to_string(),
        max_class: PublicationClass::MetadataShared,
        max_classification: DataClassification::Internal,
        publish_symbol_names: false,
        publish_source_paths: false,
        publish_signature_hashes: true,
        publish_evidence_artifacts: false,
        policy_version: 3,
        updated_at: sentinel_time_later(),
    }
}

fn publication_batch_summary() -> PublicationBatchSummary {
    PublicationBatchSummary {
        batch_id: "batch-1".to_string(),
        repository_id: FEDERATED_ID.to_string(),
        policy_version: 3,
        state: "sealed".to_string(),
        fact_count: 128,
        batch_hash: Some("deadbeef".to_string()),
        sealed_at: Some(sentinel_time()),
        acknowledged_at: None,
    }
}

fn blast_radius_report() -> BlastRadiusReport {
    BlastRadiusReport {
        seed_node_id: "node-1".to_string(),
        affected_repositories: vec![OTHER_FEDERATED_ID.to_string()],
        affected_nodes: vec![BlastRadiusNode {
            shared_node_id: "node-2".to_string(),
            repository_id: OTHER_FEDERATED_ID.to_string(),
            display_name: "billing::settle".to_string(),
            kind: "function".to_string(),
            depth: 2,
            relation_path: vec!["calls".to_string(), "imports".to_string()],
            class: PublicationClass::ContentShared,
        }],
        edge_count: 4,
        cursor: None,
        has_more: false,
    }
}

fn migration_plan_report() -> MigrationPlanReport {
    MigrationPlanReport {
        title: "Migrate payments::charge to payments::charge_v2".to_string(),
        kind: CampaignKind::ApiMigration,
        source_repository: FEDERATED_ID.to_string(),
        steps: vec![MigrationPlanStep {
            step_number: 1,
            repository_id: OTHER_FEDERATED_ID.to_string(),
            action: "update call sites".to_string(),
            target_symbols: vec!["billing::settle".to_string()],
            estimated_risk: "medium".to_string(),
        }],
        total_affected_repositories: 1,
    }
}

fn reviewer_suggestions() -> ReviewerSuggestions {
    ReviewerSuggestions {
        suggestions: vec![ReviewerSuggestion {
            reviewer_id: "dana".to_string(),
            confidence: 0.8,
            reason: "owns 3 of the 4 changed symbols".to_string(),
            relevant_symbols: vec!["payments::charge".to_string()],
            relevant_repositories: vec![FEDERATED_ID.to_string()],
        }],
    }
}

fn campaign_view() -> CampaignView {
    CampaignView {
        id: "campaign-1".to_string(),
        title: "Migrate payments::charge".to_string(),
        kind: CampaignKind::ApiMigration,
        workflow_id: "repair-api-callers".to_string(),
        state: CampaignState::Running,
        repository_count: 2,
        created_at: sentinel_time(),
        updated_at: sentinel_time_later(),
        terminal_at: None,
    }
}

fn campaign_detail_view() -> CampaignDetailView {
    CampaignDetailView {
        campaign: campaign_view(),
        repositories: vec![CampaignRepositoryView {
            campaign_id: "campaign-1".to_string(),
            repository_id: "/home/user/project".to_string(),
            federated_id: FEDERATED_ID.to_string(),
            worktree_path: Some("/home/user/worktrees/campaign-1".to_string()),
            budget_minor_units: Some(50_000),
            approval_mode: CampaignApprovalMode::PerEffect,
            state: CampaignRepoState::Running,
            enrolled_at: sentinel_time(),
            terminal_at: None,
        }],
        runs: vec![CampaignRunView {
            campaign_id: "campaign-1".to_string(),
            repository_id: "/home/user/project".to_string(),
            run_id: run_id(),
            attempt: 1,
            state: "running".to_string(),
            created_at: sentinel_time(),
            terminal_at: None,
        }],
        approvals: vec![CampaignApprovalView {
            campaign_id: "campaign-1".to_string(),
            repository_id: "/home/user/project".to_string(),
            approval_id: "50000000-0000-0000-0000-000000000001".to_string(),
            action_digest: "deadbeef".to_string(),
            decision: "approve".to_string(),
            decided_at: Some(sentinel_time_later()),
        }],
        effects: vec![CampaignEffectView {
            id: "effect-1".to_string(),
            campaign_id: "campaign-1".to_string(),
            repository_id: "/home/user/project".to_string(),
            run_id: run_id().to_string(),
            effect_kind: "patch-applied".to_string(),
            effect_digest: "cafebabe".to_string(),
            applied_at: sentinel_time_later(),
        }],
    }
}

fn graph_vectors() -> Vec<Vector> {
    vec![
        // PublicationClass ranks AUDIENCE BREADTH, and `Unknown` is the
        // NARROWEST (breadth 0) so an unrecognized class from a newer peer is
        // never published to anyone. Every variant is pinned: a reordering that
        // renamed one on the wire would move data.
        vec_of(
            "PublicationClass_PrivateLocal",
            PublicationClass::PrivateLocal,
        ),
        vec_of(
            "PublicationClass_MetadataShared",
            PublicationClass::MetadataShared,
        ),
        vec_of(
            "PublicationClass_ContentShared",
            PublicationClass::ContentShared,
        ),
        vec_of(
            "PublicationClass_OrganizationKnowledge",
            PublicationClass::OrganizationKnowledge,
        ),
        vec_of(
            "PublicationClass_PublicMarketplace",
            PublicationClass::PublicMarketplace,
        ),
        vec_of("PublicationClass_Unknown", PublicationClass::Unknown),
        vec_of("CampaignKind_ApiMigration", CampaignKind::ApiMigration),
        vec_of(
            "CampaignKind_SchemaMigration",
            CampaignKind::SchemaMigration,
        ),
        vec_of(
            "CampaignKind_DependencyUpgrade",
            CampaignKind::DependencyUpgrade,
        ),
        vec_of(
            "CampaignKind_OwnershipReview",
            CampaignKind::OwnershipReview,
        ),
        vec_of("CampaignKind_Custom", CampaignKind::Custom),
        vec_of("CampaignKind_Unknown", CampaignKind::Unknown),
        vec_of("CampaignState_Planning", CampaignState::Planning),
        vec_of("CampaignState_Running", CampaignState::Running),
        vec_of(
            "CampaignState_PartiallyFailed",
            CampaignState::PartiallyFailed,
        ),
        vec_of("CampaignState_Completed", CampaignState::Completed),
        vec_of("CampaignState_Cancelled", CampaignState::Cancelled),
        vec_of("CampaignState_Unknown", CampaignState::Unknown),
        vec_of(
            "CampaignApprovalMode_PerEffect",
            CampaignApprovalMode::PerEffect,
        ),
        vec_of("CampaignApprovalMode_PerRun", CampaignApprovalMode::PerRun),
        vec_of(
            "CampaignApprovalMode_Unknown",
            CampaignApprovalMode::Unknown,
        ),
        vec_of("CampaignRepoState_Pending", CampaignRepoState::Pending),
        vec_of("CampaignRepoState_Running", CampaignRepoState::Running),
        vec_of("CampaignRepoState_Succeeded", CampaignRepoState::Succeeded),
        vec_of("CampaignRepoState_Failed", CampaignRepoState::Failed),
        vec_of("CampaignRepoState_Denied", CampaignRepoState::Denied),
        vec_of("CampaignRepoState_Skipped", CampaignRepoState::Skipped),
        vec_of("CampaignRepoState_Unknown", CampaignRepoState::Unknown),
        vec_of("FederatedRepositoryIdentityView", federated_identity_view()),
        vec_of("GraphPublicationPolicyView", publication_policy_view()),
        vec_of(
            "UpdatePublicationPolicyRequest",
            UpdatePublicationPolicyRequest {
                max_class: Some(PublicationClass::MetadataShared),
                max_classification: Some(DataClassification::Internal),
                publish_symbol_names: Some(false),
                publish_source_paths: Some(false),
                publish_signature_hashes: Some(true),
                publish_evidence_artifacts: Some(false),
            },
        ),
        vec_of(
            // Every field absent means "change nothing" — a partial update must
            // not read as "set everything to its default", which for a policy
            // would widen the ceiling.
            "UpdatePublicationPolicyRequest_no_change",
            UpdatePublicationPolicyRequest::default(),
        ),
        vec_of("PublicationBatchSummary", publication_batch_summary()),
        vec_of("SharedNodeView", shared_node_view()),
        vec_of(
            "SharedNodeView_redacted_to_metadata_shared",
            shared_node_view_metadata_only(),
        ),
        vec_of("SharedEdgeView", shared_edge_view()),
        vec_of(
            "FederatedGraphQuery",
            FederatedGraphQuery {
                node_id: Some("node-1".to_string()),
                symbol_name: Some("payments::charge".to_string()),
                kind: Some("function".to_string()),
                language: Some("rust".to_string()),
                repository_id: Some(FEDERATED_ID.to_string()),
                class_ceiling: Some(PublicationClass::ContentShared),
                cursor: Some(page_cursor()),
                limit: Some(50),
            },
        ),
        vec_of("FederatedGraphQuery_empty", FederatedGraphQuery::default()),
        vec_of("FederatedGraphPage", federated_graph_page()),
        vec_of(
            "BlastRadiusQuery",
            BlastRadiusQuery {
                repository: "/home/user/project".to_string(),
                node_id: Some("node-1".to_string()),
                symbol_name: Some("payments::charge".to_string()),
                package: Some("payments".to_string()),
                max_depth: Some(3),
                cursor: Some(page_cursor()),
                limit: Some(50),
            },
        ),
        vec_of(
            "BlastRadiusNode",
            BlastRadiusNode {
                shared_node_id: "node-2".to_string(),
                repository_id: OTHER_FEDERATED_ID.to_string(),
                display_name: "billing::settle".to_string(),
                kind: "function".to_string(),
                depth: 2,
                relation_path: vec!["calls".to_string(), "imports".to_string()],
                class: PublicationClass::ContentShared,
            },
        ),
        vec_of("BlastRadiusReport", blast_radius_report()),
        vec_of(
            "MigrationPlanQuery",
            MigrationPlanQuery {
                source_repository: "/home/user/project".to_string(),
                source_symbol: "payments::charge".to_string(),
                target_symbol: Some("payments::charge_v2".to_string()),
                target_repositories: vec![OTHER_FEDERATED_ID.to_string()],
                kind: CampaignKind::ApiMigration,
            },
        ),
        vec_of(
            "MigrationPlanStep",
            MigrationPlanStep {
                step_number: 1,
                repository_id: OTHER_FEDERATED_ID.to_string(),
                action: "update call sites".to_string(),
                target_symbols: vec!["billing::settle".to_string()],
                estimated_risk: "medium".to_string(),
            },
        ),
        vec_of("MigrationPlanReport", migration_plan_report()),
        vec_of(
            "ReviewerSuggestionQuery",
            ReviewerSuggestionQuery {
                repository: "/home/user/project".to_string(),
                changed_symbols: vec!["payments::charge".to_string()],
                changed_paths: vec!["src/charge.rs".to_string()],
                limit: Some(5),
            },
        ),
        vec_of(
            "ReviewerSuggestion",
            ReviewerSuggestion {
                reviewer_id: "dana".to_string(),
                confidence: 0.8,
                reason: "owns 3 of the 4 changed symbols".to_string(),
                relevant_symbols: vec!["payments::charge".to_string()],
                relevant_repositories: vec![FEDERATED_ID.to_string()],
            },
        ),
        vec_of("ReviewerSuggestions", reviewer_suggestions()),
        vec_of("CampaignView", campaign_view()),
        vec_of(
            "CampaignRepoEnrollment",
            CampaignRepoEnrollment {
                repository: "/home/user/project".to_string(),
                worktree_path: Some("/home/user/worktrees/campaign-1".to_string()),
                budget_minor_units: Some(50_000),
                approval_mode: CampaignApprovalMode::PerEffect,
            },
        ),
        vec_of(
            // An unbudgeted enrollment leaves `budget_minor_units` ABSENT. A 0
            // would read as "budget exhausted" and refuse every effect.
            "CampaignRepoEnrollment_without_a_budget",
            CampaignRepoEnrollment {
                repository: "/home/user/other".to_string(),
                worktree_path: None,
                budget_minor_units: None,
                approval_mode: CampaignApprovalMode::PerRun,
            },
        ),
        vec_of(
            "CampaignRepositoryView",
            CampaignRepositoryView {
                campaign_id: "campaign-1".to_string(),
                repository_id: "/home/user/project".to_string(),
                federated_id: FEDERATED_ID.to_string(),
                worktree_path: Some("/home/user/worktrees/campaign-1".to_string()),
                budget_minor_units: Some(50_000),
                approval_mode: CampaignApprovalMode::PerEffect,
                state: CampaignRepoState::Running,
                enrolled_at: sentinel_time(),
                terminal_at: None,
            },
        ),
        vec_of(
            "CampaignRunView",
            CampaignRunView {
                campaign_id: "campaign-1".to_string(),
                repository_id: "/home/user/project".to_string(),
                run_id: run_id(),
                attempt: 1,
                state: "running".to_string(),
                created_at: sentinel_time(),
                terminal_at: None,
            },
        ),
        vec_of(
            "CampaignApprovalView",
            CampaignApprovalView {
                campaign_id: "campaign-1".to_string(),
                repository_id: "/home/user/project".to_string(),
                approval_id: "50000000-0000-0000-0000-000000000001".to_string(),
                action_digest: "deadbeef".to_string(),
                decision: "approve".to_string(),
                decided_at: Some(sentinel_time_later()),
            },
        ),
        vec_of(
            "CampaignEffectView",
            CampaignEffectView {
                id: "effect-1".to_string(),
                campaign_id: "campaign-1".to_string(),
                repository_id: "/home/user/project".to_string(),
                run_id: run_id().to_string(),
                effect_kind: "patch-applied".to_string(),
                effect_digest: "cafebabe".to_string(),
                applied_at: sentinel_time_later(),
            },
        ),
        vec_of("CampaignDetailView", campaign_detail_view()),
        vec_of(
            "CreateCampaignRequest",
            CreateCampaignRequest {
                title: "Migrate payments::charge".to_string(),
                kind: CampaignKind::ApiMigration,
                workflow_id: "repair-api-callers".to_string(),
                repositories: vec![CampaignRepoEnrollment {
                    repository: "/home/user/project".to_string(),
                    worktree_path: None,
                    budget_minor_units: Some(50_000),
                    approval_mode: CampaignApprovalMode::PerEffect,
                }],
                idempotency_key: "idem-campaign-1".to_string(),
            },
        ),
        vec_of(
            "ExecuteCampaignRequest",
            ExecuteCampaignRequest {
                campaign_id: "campaign-1".to_string(),
                retry_failed_only: true,
            },
        ),
    ]
}

// ---------------------------------------------------------------------------
// command.rs — the fourteen federation CommandBody variants.
// ---------------------------------------------------------------------------

fn command_vectors() -> Vec<Vector> {
    vec![
        vec_of(
            "CommandBody_EstablishFederatedIdentity",
            CommandBody::EstablishFederatedIdentity {
                repository: "/home/user/project".to_string(),
                display_name: Some("hello-world".to_string()),
            },
        ),
        vec_of(
            "CommandBody_GetPublicationPolicy",
            CommandBody::GetPublicationPolicy {
                repository: "/home/user/project".to_string(),
            },
        ),
        vec_of(
            "CommandBody_SetPublicationPolicy",
            CommandBody::SetPublicationPolicy {
                repository: "/home/user/project".to_string(),
                policy: UpdatePublicationPolicyRequest {
                    max_class: Some(PublicationClass::MetadataShared),
                    max_classification: Some(DataClassification::Internal),
                    publish_symbol_names: Some(false),
                    publish_source_paths: Some(false),
                    publish_signature_hashes: Some(true),
                    publish_evidence_artifacts: Some(false),
                },
            },
        ),
        vec_of(
            "CommandBody_PublishGraphFacts",
            CommandBody::PublishGraphFacts {
                repository: "/home/user/project".to_string(),
                idempotency_key: "idem-batch-1".to_string(),
            },
        ),
        vec_of(
            "CommandBody_TombstoneGraphFacts",
            CommandBody::TombstoneGraphFacts {
                repository: "/home/user/project".to_string(),
                subject_kind: "node".to_string(),
                subject_id: "node-1".to_string(),
                reason: "symbol removed".to_string(),
            },
        ),
        vec_of(
            "CommandBody_QueryFederatedGraph",
            CommandBody::QueryFederatedGraph {
                query: FederatedGraphQuery {
                    node_id: None,
                    symbol_name: Some("payments::charge".to_string()),
                    kind: None,
                    language: Some("rust".to_string()),
                    repository_id: None,
                    class_ceiling: Some(PublicationClass::ContentShared),
                    cursor: None,
                    limit: Some(50),
                },
            },
        ),
        vec_of(
            "CommandBody_QueryBlastRadius",
            CommandBody::QueryBlastRadius {
                query: BlastRadiusQuery {
                    repository: "/home/user/project".to_string(),
                    node_id: None,
                    symbol_name: Some("payments::charge".to_string()),
                    package: None,
                    max_depth: Some(3),
                    cursor: None,
                    limit: Some(50),
                },
            },
        ),
        vec_of(
            "CommandBody_PlanMigration",
            CommandBody::PlanMigration {
                query: MigrationPlanQuery {
                    source_repository: "/home/user/project".to_string(),
                    source_symbol: "payments::charge".to_string(),
                    target_symbol: Some("payments::charge_v2".to_string()),
                    target_repositories: vec![OTHER_FEDERATED_ID.to_string()],
                    kind: CampaignKind::ApiMigration,
                },
            },
        ),
        vec_of(
            "CommandBody_SuggestReviewers",
            CommandBody::SuggestReviewers {
                query: ReviewerSuggestionQuery {
                    repository: "/home/user/project".to_string(),
                    changed_symbols: vec!["payments::charge".to_string()],
                    changed_paths: vec!["src/charge.rs".to_string()],
                    limit: Some(5),
                },
            },
        ),
        vec_of(
            "CommandBody_CreateCampaign",
            CommandBody::CreateCampaign {
                campaign: CreateCampaignRequest {
                    title: "Migrate payments::charge".to_string(),
                    kind: CampaignKind::ApiMigration,
                    workflow_id: "repair-api-callers".to_string(),
                    repositories: vec![CampaignRepoEnrollment {
                        repository: "/home/user/project".to_string(),
                        worktree_path: None,
                        budget_minor_units: Some(50_000),
                        approval_mode: CampaignApprovalMode::PerEffect,
                    }],
                    idempotency_key: "idem-campaign-1".to_string(),
                },
            },
        ),
        vec_of(
            "CommandBody_GetCampaign",
            CommandBody::GetCampaign {
                campaign_id: "campaign-1".to_string(),
            },
        ),
        vec_of(
            "CommandBody_ListCampaigns",
            CommandBody::ListCampaigns {
                state: Some(CampaignState::Running),
                limit: Some(20),
            },
        ),
        vec_of(
            "CommandBody_ExecuteCampaign",
            CommandBody::ExecuteCampaign {
                request: ExecuteCampaignRequest {
                    campaign_id: "campaign-1".to_string(),
                    retry_failed_only: false,
                },
            },
        ),
        vec_of(
            "CommandBody_CancelCampaign",
            CommandBody::CancelCampaign {
                campaign_id: "campaign-1".to_string(),
            },
        ),
    ]
}

// ---------------------------------------------------------------------------
// envelope.rs — the thirteen federation Payload variants.
// ---------------------------------------------------------------------------

fn envelope_vectors() -> Vec<Vector> {
    vec![
        vec_of(
            "Payload_FederatedIdentityEstablished",
            Payload::FederatedIdentityEstablished {
                command_id: command_id(),
                identity: Box::new(federated_identity_view()),
            },
        ),
        vec_of(
            "Payload_PublicationPolicy",
            Payload::PublicationPolicy {
                command_id: command_id(),
                policy: Box::new(publication_policy_view()),
            },
        ),
        vec_of(
            "Payload_GraphFactsPublished",
            Payload::GraphFactsPublished {
                command_id: command_id(),
                summary: Box::new(publication_batch_summary()),
            },
        ),
        vec_of(
            "Payload_GraphTombstoned",
            Payload::GraphTombstoned {
                command_id: command_id(),
                tombstone_id: "tombstone-1".to_string(),
            },
        ),
        vec_of(
            "Payload_FederatedGraphResult",
            Payload::FederatedGraphResult {
                command_id: command_id(),
                page: Box::new(federated_graph_page()),
            },
        ),
        vec_of(
            "Payload_BlastRadiusResult",
            Payload::BlastRadiusResult {
                command_id: command_id(),
                report: Box::new(blast_radius_report()),
            },
        ),
        vec_of(
            "Payload_MigrationPlanResult",
            Payload::MigrationPlanResult {
                command_id: command_id(),
                report: Box::new(migration_plan_report()),
            },
        ),
        vec_of(
            "Payload_ReviewerSuggestionsResult",
            Payload::ReviewerSuggestionsResult {
                command_id: command_id(),
                suggestions: Box::new(reviewer_suggestions()),
            },
        ),
        vec_of(
            "Payload_CampaignCreated",
            Payload::CampaignCreated {
                command_id: command_id(),
                campaign_id: "campaign-1".to_string(),
            },
        ),
        vec_of(
            "Payload_CampaignDetail",
            Payload::CampaignDetail {
                command_id: command_id(),
                detail: Box::new(campaign_detail_view()),
            },
        ),
        vec_of(
            "Payload_CampaignList",
            Payload::CampaignList {
                command_id: command_id(),
                campaigns: vec![campaign_view()],
            },
        ),
        vec_of(
            "Payload_CampaignExecuted",
            Payload::CampaignExecuted {
                command_id: command_id(),
                campaign: Box::new(campaign_view()),
            },
        ),
        vec_of(
            "Payload_CampaignCancelled",
            Payload::CampaignCancelled {
                command_id: command_id(),
                campaign_id: "campaign-1".to_string(),
            },
        ),
    ]
}

// ---------------------------------------------------------------------------
// The single source of truth both the regenerator and the checks iterate.
// Paths are `/`-separated relative to `protocol-vectors/`.
// ---------------------------------------------------------------------------

fn all_files() -> Vec<(&'static str, Vec<Vector>)> {
    vec![
        ("federation/graph.json", graph_vectors()),
        ("federation/command.json", command_vectors()),
        ("federation/envelope.json", envelope_vectors()),
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Regenerate every committed federation vector file from the CURRENT types.
///
/// ```text
/// cargo test -p codypendent-protocol --test federation_vectors regenerate_vectors -- --ignored
/// ```
#[test]
#[ignore = "writes committed vector files; run explicitly to regenerate them"]
fn regenerate_vectors() {
    let dir = vectors_dir();
    for (relative, vectors) in all_files() {
        let path = dir.join(relative);
        let parent = path.parent().expect("vector path has a parent directory");
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("create {}: {e}", parent.display()));
        let text = render(&manifest_value(&vectors));
        std::fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }
}

/// CI gate #1: the committed bytes equal a fresh regeneration exactly.
#[test]
fn committed_vectors_match_current_protocol_types() {
    let dir = vectors_dir();
    for (relative, vectors) in all_files() {
        let path = dir.join(relative);
        let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e}\n\nrun `cargo test -p codypendent-protocol --test federation_vectors regenerate_vectors -- --ignored`, \
                 review the diff under protocol-vectors/, and commit it",
                path.display()
            )
        });
        let fresh = render(&manifest_value(&vectors));
        assert_eq!(
            committed,
            fresh,
            "{} is stale relative to the current protocol types.\n\
             Run `cargo test -p codypendent-protocol --test federation_vectors regenerate_vectors -- --ignored`, \
             review the diff, and commit it.",
            path.display()
        );
    }
}

/// CI gate #2: every committed entry round-trips through its own Rust type.
#[test]
fn committed_vectors_round_trip_through_their_rust_types() {
    let dir = vectors_dir();
    for (relative, vectors) in all_files() {
        let path = dir.join(relative);
        let committed_text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let committed: Value = serde_json::from_str(&committed_text)
            .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()));
        let committed_map = committed
            .as_object()
            .unwrap_or_else(|| panic!("{} is not a JSON object", path.display()));
        for vector in &vectors {
            let entry = committed_map.get(vector.name).unwrap_or_else(|| {
                panic!(
                    "{} has no entry named {:?} — run the regeneration command",
                    path.display(),
                    vector.name
                )
            });
            let reserialized = (vector.round_trip)(entry);
            assert_eq!(
                &reserialized, entry,
                "{}::{} does not round-trip through its Rust type unchanged — the wire shape \
                 changed; regenerate the vectors",
                relative, vector.name
            );
        }
    }
}

/// Every vector name registered by [`all_files`], plus the family prefix of
/// each (`"CampaignKind_Custom"` also registers `"CampaignKind"`).
fn covered_names() -> BTreeSet<String> {
    let mut covered = BTreeSet::new();
    for (_, vectors) in all_files() {
        for vector in vectors {
            covered.insert(vector.name.to_string());
            if let Some((family, _)) = vector.name.split_once('_') {
                covered.insert(family.to_string());
            }
        }
    }
    covered
}

/// Partition guard #1: every type declared in `federated_graph.rs` has at least
/// one vector. Read from source at test time, so adding a type without a vector
/// fails here rather than widening the uncovered surface in silence.
#[test]
fn every_federated_graph_type_has_a_golden_vector() {
    let path = source_file("federated_graph.rs");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    let mut declared: BTreeSet<String> = BTreeSet::new();
    for line in text.lines() {
        let trimmed = line.trim();
        for prefix in ["pub struct ", "pub enum "] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    declared.insert(name);
                }
            }
        }
    }
    assert!(
        declared.len() > 20,
        "the source scan of {} found only {} types — it stopped working, and a broken scan \
         passes vacuously",
        path.display(),
        declared.len()
    );

    let covered = covered_names();
    let uncovered: Vec<&String> = declared.iter().filter(|n| !covered.contains(*n)).collect();
    assert!(
        uncovered.is_empty(),
        "federated_graph.rs type(s) with no golden vector: {uncovered:?}\n\
         Add one in crates/protocol/tests/federation_vectors.rs and regenerate."
    );
}

/// Extract the variant names declared between the Milestone 6 marker comment
/// and the enum's trailing `#[serde(other)]` arm.
fn federation_variants(source: &str, path: &std::path::Path) -> BTreeSet<String> {
    const MARKER: &str = "// --- Milestone 6: Federation & Campaigns ---";
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let start = text.find(MARKER).unwrap_or_else(|| {
        panic!(
            "{} no longer contains {MARKER:?} — the partition guard keys off it, so either \
             restore the marker or rewrite the guard; it must not be left scanning nothing",
            path.display()
        )
    });
    let region = &text[start..];
    let end = region.find("#[serde(other)]").unwrap_or_else(|| {
        panic!(
            "{}: no `#[serde(other)]` arm after the {source} federation region — the enum lost \
             its forward-compatibility fallback, which is a fail-open change",
            path.display()
        )
    });

    let mut variants = BTreeSet::new();
    for line in region[..end].lines() {
        let trimmed = line.trim();
        // A variant declaration at enum level: `Name {` (all federation
        // variants carry fields), never a field, attribute or doc line.
        if let Some(name) = trimmed.strip_suffix(" {") {
            // ⚡ Bolt: skip UTF-8 decoding for ASCII checks
            if !name.is_empty()
                && name.as_bytes().first().is_some_and(|&c| c.is_ascii_uppercase())
                && name.as_bytes().iter().all(|&c| c.is_ascii_alphanumeric())
            {
                variants.insert(name.to_string());
            }
        }
    }
    variants
}

/// Partition guard #2: every federation `CommandBody` and `Payload` variant
/// declared in the crate's own source has a vector. This is the guard that
/// matters most — a new federation command is exactly the thing that gets added
/// without anyone remembering the vectors, and without this it would be
/// invisible.
#[test]
fn every_federation_command_and_payload_variant_has_a_golden_vector() {
    let covered = covered_names();

    let commands = federation_variants("CommandBody", &source_file("command.rs"));
    assert!(
        commands.len() >= 14,
        "the CommandBody federation scan found only {} variants — expected at least the \
         fourteen that exist; the scan stopped working",
        commands.len()
    );
    let missing_commands: Vec<String> = commands
        .iter()
        .filter(|name| !covered.contains(&format!("CommandBody_{name}")))
        .cloned()
        .collect();
    assert!(
        missing_commands.is_empty(),
        "federation CommandBody variant(s) with no golden vector: {missing_commands:?}\n\
         Add `CommandBody_<Variant>` in crates/protocol/tests/federation_vectors.rs and \
         regenerate. (Reachability is separate: a new client-issued command also needs a \
         DECIDED role floor in crates/daemon/src/commands.rs `role_permits` and real \
         `named_resources()`.)"
    );

    let payloads = federation_variants("Payload", &source_file("envelope.rs"));
    assert!(
        payloads.len() >= 13,
        "the Payload federation scan found only {} variants — expected at least the thirteen \
         that exist; the scan stopped working",
        payloads.len()
    );
    let missing_payloads: Vec<String> = payloads
        .iter()
        .filter(|name| !covered.contains(&format!("Payload_{name}")))
        .cloned()
        .collect();
    assert!(
        missing_payloads.is_empty(),
        "federation Payload variant(s) with no golden vector: {missing_payloads:?}\n\
         Add `Payload_<Variant>` in crates/protocol/tests/federation_vectors.rs and regenerate."
    );
}

/// The rule the vectors cannot state on their own: an unrecognized tag from a
/// newer peer decodes to `Unknown`, and for `PublicationClass` `Unknown` must be
/// the NARROWEST audience (breadth 0), never a middle guess — otherwise a class
/// this build has never heard of would authorize publication.
#[test]
fn an_unrecognized_publication_class_is_the_narrowest_audience() {
    let class: PublicationClass = serde_json::from_str("\"interplanetary-shared\"").unwrap();
    assert_eq!(class, PublicationClass::Unknown);
    assert_eq!(class.breadth(), 0);
    assert_eq!(
        class.strictest(PublicationClass::PublicMarketplace),
        PublicationClass::Unknown
    );
    assert!(class < PublicationClass::PrivateLocal);

    let kind: CampaignKind = serde_json::from_str("\"telepathy-migration\"").unwrap();
    assert_eq!(kind, CampaignKind::Unknown);
    let state: CampaignState = serde_json::from_str("\"levitating\"").unwrap();
    assert_eq!(state, CampaignState::Unknown);
    let mode: CampaignApprovalMode = serde_json::from_str("\"per-vibe\"").unwrap();
    assert_eq!(mode, CampaignApprovalMode::Unknown);
    let repo_state: CampaignRepoState = serde_json::from_str("\"vanished\"").unwrap();
    assert_eq!(repo_state, CampaignRepoState::Unknown);
}
