//! Milestone 6: the daemon half of cross-repository federation.
//!
//! `crates/federation` already holds the algebra — publication classes,
//! classification inheritance, tombstone precedence, access-safe traversal and
//! the campaign coordinator — over migrations `0047_graph_publication.sql` and
//! `0048_multi_repo_campaigns.sql`. It had no caller: every M6 command fell
//! through `commands::apply`'s catch-all. This module is that caller.
//!
//! # Identity
//!
//! `stable_repository_id` (`crates/knowledge/src/codegraph.rs:174`) is the
//! SHA-256 of the **canonical local path**. Two clones of one repository get
//! different ids and the same path on two machines collides, so it is a local
//! partition key and nothing else. Everything that could ever cross a machine
//! boundary — `shared_node_id`, `shared_edge_id`, a campaign's enrolment
//! snapshot — is keyed off
//! [`FederatedRepositoryIdentity::federated_id`](codypendent_federation::FederatedRepositoryIdentity),
//! which is `SHA-256(root_commit || '\n' || normalized_remote)` and therefore
//! identical across checkouts and machines. The local id survives here only as
//! the join key to `code_nodes.repository`, exactly as migration 0047 says.
//!
//! # What actually leaves the machine
//!
//! `graph_publication_batch` seals a Merkle-rooted batch and `graph_tombstone`
//! records retractions. Sealed batch metadata and retractions are copied into
//! the durable control-plane outbox, while the projection tables remain local.
//! The policy gate is enforced before that enqueue: an absent or unreadable
//! `graph_publication_policy` publishes NOTHING, a class the policy does not
//! permit is recorded `withheld-class` rather than projected, and a repository
//! with no normalized remote cannot publish above `metadata-shared` (two
//! unrelated local-only repositories that share a root commit would otherwise
//! collide).
//!
//! # Ownership
//!
//! Every command here names either a repository **path** or a campaign id.
//! Neither is a resource `CommandBody::named_resources` can resolve — it
//! classifies all of M6 as `NamedResource::DaemonStore(CodeGraph)`, which only
//! answers "may this principal address the federated store at all". The
//! per-repository and per-campaign resolution therefore happens in
//! [`authorize`], called from `server::authorize_command` — the same choke
//! point, not a per-arm check — and every refusal is the one generic
//! [`repository_not_found`] / [`campaign_not_found`] value, so naming an id can
//! never confirm it exists somewhere else.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Utc};
use codypendent_federation::{
    AuthorizedGrants, BatchState, CampaignApprovalMode as FedApprovalMode, CampaignEngine,
    CampaignKind as FedCampaignKind, FederatedRepositoryIdentity, FederationPageCursor,
    PublicationClass, PublicationDecision, PublicationPolicy, RepositoryGrant, SharedGraphQuery,
    SharedGraphStore, SubjectKind, TargetRepositorySpec, TombstoneManager,
};
use codypendent_protocol::federated_graph as wire;
use codypendent_protocol::{
    ClientId, CodeNodeId, CodypendentError, CommandBody, CommandId, Payload, RepositoryId,
};
use sqlx::Row as _;
use tokio::process::Command;

use crate::principal::PeerPrincipal;
use crate::server::{principal_owns_repository, ServerState};
use crate::workflows::StartWorkflowRequest;

/// The most facts one `PublishGraphFacts` will project in a single pass. A
/// repository larger than this seals a partial batch rather than holding a
/// socket open for minutes; the next call resumes where policy leaves off
/// (projection is keyed by content hash, so re-projecting is a no-op upsert).
const MAX_PUBLICATION_FACTS: i64 = 50_000;

/// Default traversal depth for a blast radius when the client names none.
const DEFAULT_BLAST_RADIUS_DEPTH: usize = 4;

/// Hard ceiling on traversal depth, so a client cannot ask for a full-graph walk.
const MAX_BLAST_RADIUS_DEPTH: usize = 12;

/// Default page size for `QueryFederatedGraph`.
const DEFAULT_PAGE_LIMIT: u32 = 100;

/// Hard ceiling on a page.
const MAX_PAGE_LIMIT: u32 = 500;

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// The single refusal every repository-addressed federation command answers
/// with when the path is not a checkout, is not one this principal owns, or has
/// no federated identity established.
///
/// Deliberately carries no path and no distinction — modelled on
/// [`crate::codegraph::node_not_found`]. If a future edit echoes the repository
/// back, `GetPublicationPolicy` becomes an oracle that confirms another user's
/// checkouts one probe at a time.
#[must_use]
pub(crate) fn repository_not_found() -> CodypendentError {
    CodypendentError::new(
        "federation.repository-not-found",
        "no federated repository".to_string(),
        false,
    )
}

/// The campaign equivalent of [`repository_not_found`]: "no such campaign" and
/// "a campaign owned by another principal" are one answer.
#[must_use]
pub(crate) fn campaign_not_found() -> CodypendentError {
    CodypendentError::new(
        "federation.campaign-not-found",
        "no campaign".to_string(),
        false,
    )
}

fn invalid(message: impl Into<String>) -> CodypendentError {
    CodypendentError::new("federation.invalid-request", message.into(), false)
}

fn internal(error: impl std::fmt::Display) -> CodypendentError {
    CodypendentError::new("federation.internal-error", error.to_string(), true)
}

// ---------------------------------------------------------------------------
// Command classification
// ---------------------------------------------------------------------------

/// Whether `body` is one of the Milestone 6 federation commands this module
/// serves. Used by the connection loop to intercept them before the generic
/// write path (they act on stores outside the session ledger, exactly as the
/// marketplace and secret commands do).
#[must_use]
pub(crate) fn is_federation_command(body: &CommandBody) -> bool {
    matches!(
        body,
        CommandBody::EstablishFederatedIdentity { .. }
            | CommandBody::GetPublicationPolicy { .. }
            | CommandBody::SetPublicationPolicy { .. }
            | CommandBody::PublishGraphFacts { .. }
            | CommandBody::TombstoneGraphFacts { .. }
            | CommandBody::QueryFederatedGraph { .. }
            | CommandBody::QueryBlastRadius { .. }
            | CommandBody::PlanMigration { .. }
            | CommandBody::SuggestReviewers { .. }
            | CommandBody::CreateCampaign { .. }
            | CommandBody::GetCampaign { .. }
            | CommandBody::ListCampaigns { .. }
            | CommandBody::ExecuteCampaign { .. }
            | CommandBody::CancelCampaign { .. }
    )
}

/// Whether `body` is a federation **read** — issuable by every attached role,
/// including `Observer`, whose whole job is to watch. Every one of these is
/// already ownership-scoped: a repository read resolves through
/// [`authorize`], a graph query returns only rows inside the principal's
/// [`AuthorizedGrants`], and a campaign read is scoped to `owner_uid`.
#[must_use]
fn is_federation_read(body: &CommandBody) -> bool {
    matches!(
        body,
        CommandBody::GetPublicationPolicy { .. }
            | CommandBody::QueryFederatedGraph { .. }
            | CommandBody::QueryBlastRadius { .. }
            | CommandBody::PlanMigration { .. }
            | CommandBody::SuggestReviewers { .. }
            | CommandBody::GetCampaign { .. }
            | CommandBody::ListCampaigns { .. }
    )
}

// ---------------------------------------------------------------------------
// Ownership resolution — called from THE gate
// ---------------------------------------------------------------------------

/// Resolve every repository path and campaign id `body` names against this
/// principal, in the daemon's own storage.
///
/// `Ok(None)` means carry on; `Ok(Some(err))` is the generic not-found the
/// caller must reject with. Called from `server::authorize_command` so it runs
/// once, before dispatch, for every federation command — the alternative
/// (a check inside each handler) is exactly the per-arm discipline that leaked
/// `PublishDocument`.
pub(crate) async fn authorize(
    state: &ServerState,
    principal: PeerPrincipal,
    body: &CommandBody,
) -> anyhow::Result<Option<CodypendentError>> {
    // Repository-addressed commands. `EstablishFederatedIdentity` is the one
    // that may run before an identity exists, so it checks path ownership only;
    // every other command additionally requires the identity to be established,
    // and both failures collapse to the same refusal.
    let (repository, require_identity) = match body {
        CommandBody::EstablishFederatedIdentity { repository, .. } => (Some(repository), false),
        CommandBody::GetPublicationPolicy { repository }
        | CommandBody::SetPublicationPolicy { repository, .. }
        | CommandBody::PublishGraphFacts { repository, .. }
        | CommandBody::TombstoneGraphFacts { repository, .. } => (Some(repository), true),
        CommandBody::QueryBlastRadius { query } => (Some(&query.repository), true),
        CommandBody::PlanMigration { query } => (Some(&query.source_repository), true),
        CommandBody::SuggestReviewers { query } => (Some(&query.repository), true),
        _ => (None, false),
    };
    if let Some(repository) = repository {
        if resolve_owned_repository(state, principal, repository, require_identity)
            .await?
            .is_none()
        {
            return Ok(Some(repository_not_found()));
        }
    }

    // Campaign-addressed commands. `campaigns.owner_uid` is kernel-derived at
    // creation, so it is the authority here.
    let campaign_id = match body {
        CommandBody::GetCampaign { campaign_id } | CommandBody::CancelCampaign { campaign_id } => {
            Some(campaign_id.as_str())
        }
        CommandBody::ExecuteCampaign { request } => Some(request.campaign_id.as_str()),
        _ => None,
    };
    if let Some(campaign_id) = campaign_id {
        if !principal_owns_campaign(state, principal, campaign_id).await? {
            return Ok(Some(campaign_not_found()));
        }
    }

    // A campaign enrols repositories by path; each one must be owned AND have a
    // federated identity, because the enrolment snapshots `federated_id` and an
    // unowned path would let a campaign aim a workflow run at another user's
    // checkout.
    if let CommandBody::CreateCampaign { campaign } = body {
        for enrolment in &campaign.repositories {
            if resolve_owned_repository(state, principal, &enrolment.repository, true)
                .await?
                .is_none()
            {
                return Ok(Some(repository_not_found()));
            }
        }
    }

    Ok(None)
}

/// Whether `campaign_id` names a campaign this principal owns. Deny-first: an
/// unknown id and another principal's campaign are both `false`.
pub(crate) async fn principal_owns_campaign(
    state: &ServerState,
    principal: PeerPrincipal,
    campaign_id: &str,
) -> anyhow::Result<bool> {
    let owner: Option<i64> = sqlx::query_scalar("SELECT owner_uid FROM campaigns WHERE id = ?")
        .bind(campaign_id)
        .fetch_optional(&state.pool)
        .await?;
    Ok(owner
        .and_then(|uid| u32::try_from(uid).ok())
        .is_some_and(|uid| principal.owns(uid)))
}

/// A repository path this principal owns, resolved to its local
/// [`RepositoryId`] and (when `require_identity`) its federated identity.
async fn resolve_owned_repository(
    state: &ServerState,
    principal: PeerPrincipal,
    repository: &str,
    require_identity: bool,
) -> anyhow::Result<Option<ResolvedRepository>> {
    let Some(root) = git_toplevel(Path::new(repository)).await else {
        return Ok(None);
    };
    if !principal_owns_repository(state, principal, &root).await? {
        return Ok(None);
    }
    let repository_id = codypendent_knowledge::stable_repository_id(&root);
    let identity = store(state)
        .get_identity(&repository_id)
        .await
        .ok()
        .flatten();
    if require_identity && identity.is_none() {
        return Ok(None);
    }
    Ok(Some(ResolvedRepository {
        root,
        repository_id,
        identity,
    }))
}

/// An owned repository path, resolved.
struct ResolvedRepository {
    /// The canonical Git toplevel — never the path the client sent.
    root: PathBuf,
    /// The path-derived local id (`code_nodes.repository`). NEVER published.
    repository_id: RepositoryId,
    /// The cross-machine identity, when one has been established.
    identity: Option<FederatedRepositoryIdentity>,
}

fn store(state: &ServerState) -> SharedGraphStore {
    SharedGraphStore::new(state.pool.clone())
}

// ---------------------------------------------------------------------------
// Grants
// ---------------------------------------------------------------------------

/// The repositories this principal may see federated facts from, each ceilinged
/// by that repository's own publication policy.
///
/// A repository is in the set when it is the root of a session this principal
/// owns (the same derivation `principal_owns_repository` uses) **and** it has a
/// federated identity. The ceiling is the policy's — a fact can never be read
/// back at a wider audience than the policy that published it allows, and an
/// absent policy contributes the strictest possible grant (`private-local` /
/// `internal`), which authorizes no published fact at all.
async fn grants_for_principal(
    state: &ServerState,
    principal: PeerPrincipal,
) -> anyhow::Result<AuthorizedGrants> {
    let store = store(state);
    let mut grants = Vec::new();
    let mut seen = HashSet::new();
    for root in owned_repository_roots(state, principal).await? {
        let repository_id = codypendent_knowledge::stable_repository_id(&root);
        if !seen.insert(repository_id) {
            continue;
        }
        let Ok(Some(identity)) = store.get_identity(&repository_id).await else {
            continue;
        };
        let policy = store
            .get_policy(&repository_id)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| PublicationPolicy::private_default(repository_id, 0));
        grants.push(RepositoryGrant {
            repository_id,
            federated_id: identity.federated_id,
            max_class: policy.max_class,
            max_classification: policy.max_classification,
        });
    }
    Ok(AuthorizedGrants::new(i64::from(principal.uid()), grants))
}

/// The canonical repository roots of every session this principal owns.
async fn owned_repository_roots(
    state: &ServerState,
    principal: PeerPrincipal,
) -> anyhow::Result<Vec<PathBuf>> {
    let owned: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM sessions WHERE COALESCE(owner_uid, ?) = ?")
            .bind(i64::from(state.daemon_uid))
            .bind(i64::from(principal.uid()))
            .fetch_all(&state.pool)
            .await?;
    // De-duplicate the recovered paths BEFORE resolving them: several sessions
    // in one checkout are the norm, and each resolution is a `git` spawn.
    let mut repositories = HashSet::new();
    for (id,) in owned {
        let Ok(session_id) = codypendent_protocol::SessionId::from_str(&id) else {
            continue;
        };
        let provenance = crate::commands::session_run_provenance(&state.pool, session_id).await?;
        if let Some(repository) = provenance.repository {
            repositories.insert(repository);
        }
    }
    let mut roots = Vec::new();
    for repository in repositories {
        if let Some(root) = git_toplevel(Path::new(&repository)).await {
            roots.push(root);
        }
    }
    Ok(roots)
}

// ---------------------------------------------------------------------------
// Git
// ---------------------------------------------------------------------------

/// `git rev-parse --show-toplevel`, canonicalized — the same derivation
/// `codypendentd::scan::repository_id_for` and
/// `codypendent_knowledge::anchor_repository_id` use, so the local
/// [`RepositoryId`] this module computes is byte-identical to the one the code
/// graph stored. `None` outside a checkout.
async fn git_toplevel(path: &Path) -> Option<PathBuf> {
    let out = git(path, &["rev-parse", "--show-toplevel"]).await?;
    let root = PathBuf::from(out.trim());
    if root.as_os_str().is_empty() {
        return None;
    }
    Some(root.canonicalize().unwrap_or(root))
}

/// The repository's earliest root commit, chosen deterministically (sorted, not
/// "whatever `rev-list` printed first") so two clones of one history derive the
/// same federated id even when the history has several roots.
async fn root_commit(root: &Path) -> Option<String> {
    let out = git(root, &["rev-list", "--max-parents=0", "HEAD"]).await?;
    let mut roots: Vec<&str> = out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    roots.sort_unstable();
    roots.first().map(|c| (*c).to_string())
}

/// The raw `origin` URL, or `None` for a repository with no remote. Normalized
/// by [`codypendent_federation::normalize_remote`] inside
/// `FederatedRepositoryIdentity::new`, never here.
async fn origin_remote(root: &Path) -> Option<String> {
    let out = git(root, &["config", "--get", "remote.origin.url"]).await?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Spawn `git` with an explicit argument vector (never a shell string) in `dir`.
async fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Apply one federation command.
///
/// These bodies are intercepted at the connection level, BEFORE the generic
/// write path, so `commands::role_permits` never runs for them — its floor is
/// re-applied here or it is dead code, exactly as
/// `server::handle_marketplace_command` re-applies its own. Role is checked
/// AFTER `server::authorize_command`, so a `role-denied` here only ever
/// concerns a repository or campaign the principal already owns.
pub(crate) async fn handle(
    state: &ServerState,
    principal: PeerPrincipal,
    role: codypendent_protocol::ClientRole,
    client_id: Option<ClientId>,
    command_id: CommandId,
    body: &CommandBody,
) -> Payload {
    if !is_federation_read(body) && role != codypendent_protocol::ClientRole::Controller {
        return Payload::CommandRejected(CodypendentError::new(
            "protocol.role-denied",
            "changing federated identity, publication policy, published facts or campaigns \
             requires the Controller role",
            false,
        ));
    }
    match dispatch(state, principal, client_id, command_id, body).await {
        Ok(payload) => payload,
        Err(error) => Payload::CommandRejected(error),
    }
}

async fn dispatch(
    state: &ServerState,
    principal: PeerPrincipal,
    client_id: Option<ClientId>,
    command_id: CommandId,
    body: &CommandBody,
) -> Result<Payload, CodypendentError> {
    match body {
        CommandBody::EstablishFederatedIdentity {
            repository,
            display_name,
        } => {
            establish_identity(
                state,
                principal,
                command_id,
                repository,
                display_name.as_deref(),
            )
            .await
        }
        CommandBody::GetPublicationPolicy { repository } => {
            get_policy(state, principal, command_id, repository).await
        }
        CommandBody::SetPublicationPolicy { repository, policy } => {
            set_policy(state, principal, command_id, repository, policy).await
        }
        CommandBody::PublishGraphFacts {
            repository,
            idempotency_key,
        } => publish_facts(state, principal, command_id, repository, idempotency_key).await,
        CommandBody::TombstoneGraphFacts {
            repository,
            subject_kind,
            subject_id,
            reason,
        } => {
            tombstone_facts(
                state,
                principal,
                command_id,
                repository,
                subject_kind,
                subject_id,
                reason,
            )
            .await
        }
        CommandBody::QueryFederatedGraph { query } => {
            query_graph(state, principal, command_id, query).await
        }
        CommandBody::QueryBlastRadius { query } => {
            blast_radius(state, principal, command_id, query).await
        }
        CommandBody::PlanMigration { query } => {
            plan_migration(state, principal, command_id, query).await
        }
        CommandBody::SuggestReviewers { query } => {
            suggest_reviewers(state, principal, command_id, query).await
        }
        CommandBody::CreateCampaign { campaign } => {
            create_campaign(state, principal, command_id, campaign).await
        }
        CommandBody::GetCampaign { campaign_id } => {
            get_campaign(state, principal, command_id, campaign_id).await
        }
        CommandBody::ListCampaigns {
            state: filter,
            limit,
        } => list_campaigns(state, principal, command_id, *filter, *limit).await,
        CommandBody::ExecuteCampaign { request } => {
            execute_campaign(state, principal, client_id, command_id, request).await
        }
        CommandBody::CancelCampaign { campaign_id } => {
            cancel_campaign(state, command_id, campaign_id).await
        }
        // Unreachable: the connection loop only routes `is_federation_command`
        // bodies here. Restated defensively rather than panicking.
        _ => Err(invalid("not a federation command")),
    }
}

/// Re-resolve a repository inside a handler. `authorize` has already refused
/// anything this principal does not own, so a `None` here means the checkout
/// vanished between the gate and the handler; it answers the same refusal.
async fn resolved(
    state: &ServerState,
    principal: PeerPrincipal,
    repository: &str,
    require_identity: bool,
) -> Result<ResolvedRepository, CodypendentError> {
    resolve_owned_repository(state, principal, repository, require_identity)
        .await
        .map_err(internal)?
        .ok_or_else(repository_not_found)
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

async fn establish_identity(
    state: &ServerState,
    principal: PeerPrincipal,
    command_id: CommandId,
    repository: &str,
    display_name: Option<&str>,
) -> Result<Payload, CodypendentError> {
    let resolved = resolved(state, principal, repository, false).await?;
    let Some(root_commit) = root_commit(&resolved.root).await else {
        return Err(CodypendentError::new(
            "federation.no-root-commit",
            "this repository has no commits, so it has no durable federated identity yet",
            false,
        ));
    };
    let remote = origin_remote(&resolved.root).await;
    let label = display_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            resolved
                .root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "repository".to_string())
        });

    let identity = FederatedRepositoryIdentity::new(
        resolved.repository_id,
        root_commit,
        remote.as_deref(),
        label,
        i64::from(principal.uid()),
    );
    store(state)
        .upsert_identity(&identity)
        .await
        .map_err(internal)?;

    Ok(Payload::FederatedIdentityEstablished {
        command_id,
        identity: Box::new(identity_view(&identity)),
    })
}

fn identity_view(identity: &FederatedRepositoryIdentity) -> wire::FederatedRepositoryIdentityView {
    wire::FederatedRepositoryIdentityView {
        repository_id: identity.repository_id.to_string(),
        federated_id: identity.federated_id.clone(),
        root_commit: identity.root_commit.clone(),
        normalized_remote: identity.normalized_remote.clone(),
        display_name: identity.display_name.clone(),
        established_at: identity.established_at,
    }
}

// ---------------------------------------------------------------------------
// Publication policy
// ---------------------------------------------------------------------------

async fn get_policy(
    state: &ServerState,
    principal: PeerPrincipal,
    command_id: CommandId,
    repository: &str,
) -> Result<Payload, CodypendentError> {
    let resolved = resolved(state, principal, repository, true).await?;
    // Migration 0047: "Absent row == 'private-local' == publish nothing." The
    // absent case is reported as that policy rather than as an error, so an
    // operator can see the effective ceiling without first having to write one.
    let policy = store(state)
        .get_policy(&resolved.repository_id)
        .await
        .map_err(internal)?
        .unwrap_or_else(|| PublicationPolicy::private_default(resolved.repository_id, 0));
    Ok(Payload::PublicationPolicy {
        command_id,
        policy: Box::new(policy_view(&policy)),
    })
}

async fn set_policy(
    state: &ServerState,
    principal: PeerPrincipal,
    command_id: CommandId,
    repository: &str,
    request: &wire::UpdatePublicationPolicyRequest,
) -> Result<Payload, CodypendentError> {
    let resolved = resolved(state, principal, repository, true).await?;
    let store = store(state);
    let existing = store
        .get_policy(&resolved.repository_id)
        .await
        .map_err(internal)?;

    let base = existing
        .clone()
        .unwrap_or_else(|| PublicationPolicy::private_default(resolved.repository_id, 0));
    let next = PublicationPolicy {
        repository_id: resolved.repository_id,
        max_class: request.max_class.unwrap_or(base.max_class),
        max_classification: request
            .max_classification
            .unwrap_or(base.max_classification),
        publish_symbol_names: request
            .publish_symbol_names
            .unwrap_or(base.publish_symbol_names),
        publish_source_paths: request
            .publish_source_paths
            .unwrap_or(base.publish_source_paths),
        publish_signature_hashes: request
            .publish_signature_hashes
            .unwrap_or(base.publish_signature_hashes),
        publish_evidence_artifacts: request
            .publish_evidence_artifacts
            .unwrap_or(base.publish_evidence_artifacts),
        // Monotonic: migration 0047 stamps this onto every publication record so
        // an audit can answer "under which policy did this leave?", and so a
        // tightening can find every row published under a looser version.
        policy_version: existing.as_ref().map_or(1, |p| p.policy_version + 1),
        updated_at: Utc::now(),
        updated_by_uid: i64::from(principal.uid()),
    };
    store.upsert_policy(&next).await.map_err(internal)?;

    // A policy change re-derives every incident edge's inherited class and
    // tombstones the ones that narrowed. Without this a node narrowed from
    // content-shared to private-local leaves its edges published at the old,
    // wider class — and the tombstone precedence rule then blocks the next seal
    // until the retraction is drained, which is the intended ordering.
    store
        .reclassify_edges_for_repository(&resolved.repository_id, i64::from(principal.uid()))
        .await
        .map_err(internal)?;
    enqueue_graph_tombstones(state, &resolved.repository_id.to_string()).await?;

    Ok(Payload::PublicationPolicy {
        command_id,
        policy: Box::new(policy_view(&next)),
    })
}

fn policy_view(policy: &PublicationPolicy) -> wire::GraphPublicationPolicyView {
    wire::GraphPublicationPolicyView {
        repository_id: policy.repository_id.to_string(),
        max_class: policy.max_class,
        max_classification: policy.max_classification,
        publish_symbol_names: policy.publish_symbol_names,
        publish_source_paths: policy.publish_source_paths,
        publish_signature_hashes: policy.publish_signature_hashes,
        publish_evidence_artifacts: policy.publish_evidence_artifacts,
        policy_version: u64::try_from(policy.policy_version).unwrap_or(0),
        updated_at: policy.updated_at,
    }
}

// ---------------------------------------------------------------------------
// Publication
// ---------------------------------------------------------------------------

/// One local code-graph node, as read for projection.
struct LocalNode {
    id: CodeNodeId,
    symbol_key: String,
    kind: String,
    language: String,
    package: Option<String>,
    qualified_name: Option<String>,
    signature_hash: Option<String>,
    revision: String,
}

async fn publish_facts(
    state: &ServerState,
    principal: PeerPrincipal,
    command_id: CommandId,
    repository: &str,
    idempotency_key: &str,
) -> Result<Payload, CodypendentError> {
    let resolved = resolved(state, principal, repository, true).await?;
    let identity = resolved.identity.clone().ok_or_else(repository_not_found)?;
    let store = store(state);
    let uid = i64::from(principal.uid());

    // FAIL CLOSED. `graph_publication_policy` is the gate deciding what leaves
    // the machine: an absent row publishes nothing, and so does an unreadable
    // one (the `?` below surfaces the read failure rather than falling back to
    // a permissive default).
    let policy = store
        .get_policy(&resolved.repository_id)
        .await
        .map_err(internal)?;

    let key = if idempotency_key.trim().is_empty() {
        // A client that sends none still gets batch-level idempotency, keyed to
        // the repository and the policy version it is publishing under.
        format!(
            "publish:{}:v{}",
            resolved.repository_id,
            policy.as_ref().map_or(0, |p| p.policy_version)
        )
    } else {
        idempotency_key.to_string()
    };

    let (batch, _created) = store
        .create_batch_idempotent(
            &resolved.repository_id,
            uid,
            &key,
            policy.as_ref().map_or(1, |p| p.policy_version),
        )
        .await
        .map_err(internal)?;

    // A batch already sealed or acknowledged is a replayed delivery: return it
    // verbatim rather than projecting a second time. Reconcile its outbox row
    // first so retry also repairs a process exit between seal and enqueue.
    if batch.state != BatchState::Building {
        enqueue_graph_batch(state, &batch.id).await?;
        return Ok(Payload::GraphFactsPublished {
            command_id,
            summary: Box::new(batch_summary(&batch)),
        });
    }

    let Some(policy) = policy else {
        // No policy: seal an empty batch. The batch exists so the audit trail
        // records that a publication was attempted and produced nothing.
        let sealed = store.seal_batch(&batch.id).await.map_err(seal_error)?;
        enqueue_graph_batch(state, &sealed.id).await?;
        return Ok(Payload::GraphFactsPublished {
            command_id,
            summary: Box::new(batch_summary(&sealed)),
        });
    };

    // The candidate audience is the operator's ceiling. `project_node` narrows
    // it further with `strictest()` and refuses outright below
    // `metadata-shared` (a `private-local` policy therefore publishes nothing)
    // and above `metadata-shared` for a repository with no normalized remote.
    let candidate_class = policy.max_class;
    // Sensitivity is the policy's ceiling. Nothing in `code_nodes` carries a
    // per-node classification, so there is no evidence on which to RAISE this —
    // and a derived classification may only ever raise, never lower. If a future
    // extractor supplies one, it is combined here with a max() and never a min().
    let candidate_classification = policy.max_classification;

    let nodes = load_local_nodes(state, &resolved.repository_id).await?;
    let mut published: HashMap<CodeNodeId, codypendent_federation::PublishedNode> = HashMap::new();

    for node in &nodes {
        let (projected, decision) = store
            .project_node(
                &identity,
                Some(&policy),
                &node.id,
                &node.symbol_key,
                &node.kind,
                &node.language,
                node.package.as_deref(),
                node.qualified_name.as_deref(),
                // `code_nodes` carries no source path (migration 0003), so there
                // is nothing to publish here even when the policy permits it.
                None,
                node.signature_hash.as_deref(),
                candidate_class,
                candidate_classification,
                &node.revision,
            )
            .await
            .map_err(internal)?;

        store
            .record_publication(
                &batch.id,
                SubjectKind::Node,
                &projected.shared_node_id,
                &resolved.repository_id,
                projected.class,
                projected.classification,
                decision,
                policy.policy_version,
                &projected.content_hash,
                "none",
                "default",
                uid,
            )
            .await
            .map_err(internal)?;

        if decision == PublicationDecision::Published {
            published.insert(node.id, projected);
        } else {
            // Withheld: the decision is recorded (so an operator can see WHY a
            // fact never left) but the projection row must not survive, or a
            // later query would serve a fact the policy refused.
            sqlx::query("DELETE FROM shared_graph_node WHERE shared_node_id = ?")
                .bind(&projected.shared_node_id)
                .execute(&state.pool)
                .await
                .map_err(internal)?;
        }
    }

    if !published.is_empty() {
        for edge in load_local_edges(state, &resolved.repository_id).await? {
            let (Some(from), Some(to)) = (published.get(&edge.from), published.get(&edge.to))
            else {
                continue;
            };
            let (projected, decision) = store
                .project_edge(
                    from,
                    to,
                    Some(&policy),
                    Some(&policy),
                    &edge.relation,
                    edge.confidence,
                    &edge.evidence_kind,
                    edge.evidence_artifact.as_deref(),
                    // The same evidence floor `reclassify_edges_for_repository`
                    // recomputes with. They MUST match: a different floor here
                    // would make every policy write see a digest mismatch and
                    // tombstone edges that never actually narrowed.
                    PublicationClass::MetadataShared,
                    &edge.revision,
                )
                .await
                .map_err(internal)?;

            store
                .record_publication(
                    &batch.id,
                    SubjectKind::Edge,
                    &projected.shared_edge_id,
                    &resolved.repository_id,
                    projected.class,
                    projected.classification,
                    decision,
                    policy.policy_version,
                    &projected.content_hash,
                    "none",
                    "default",
                    uid,
                )
                .await
                .map_err(internal)?;

            if decision != PublicationDecision::Published {
                sqlx::query("DELETE FROM shared_graph_edge WHERE shared_edge_id = ?")
                    .bind(&projected.shared_edge_id)
                    .execute(&state.pool)
                    .await
                    .map_err(internal)?;
            }
        }
    }

    let sealed = store.seal_batch(&batch.id).await.map_err(seal_error)?;
    enqueue_graph_batch(state, &sealed.id).await?;
    Ok(Payload::GraphFactsPublished {
        command_id,
        summary: Box::new(batch_summary(&sealed)),
    })
}

/// Sealing is refused while unacknowledged tombstones exist, so a consumer can
/// never see a new batch resurrect a fact it was told to drop. Surfaced as its
/// own retryable code: draining the tombstones makes the identical retry work.
fn seal_error(error: codypendent_federation::FederationError) -> CodypendentError {
    match error {
        codypendent_federation::FederationError::UnacknowledgedTombstonesPending => {
            CodypendentError::new(
                "federation.tombstones-pending",
                "unacknowledged tombstones must be drained before a new batch is sealed",
                true,
            )
        }
        other => internal(other),
    }
}

async fn enqueue_graph_batch(state: &ServerState, batch_id: &str) -> Result<(), CodypendentError> {
    let mut tx = state.pool.begin().await.map_err(internal)?;
    crate::control_plane_sync::outbox::enqueue_graph_batch_snapshot(&mut tx, batch_id)
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(())
}

async fn enqueue_graph_tombstones(
    state: &ServerState,
    repository_id: &str,
) -> Result<(), CodypendentError> {
    let mut tx = state.pool.begin().await.map_err(internal)?;
    crate::control_plane_sync::outbox::enqueue_graph_tombstones_for_repository(
        &mut tx,
        repository_id,
    )
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(())
}

fn batch_summary(
    batch: &codypendent_federation::PublicationBatch,
) -> wire::PublicationBatchSummary {
    wire::PublicationBatchSummary {
        batch_id: batch.id.clone(),
        repository_id: batch.repository_id.to_string(),
        policy_version: u64::try_from(batch.policy_version).unwrap_or(0),
        state: batch.state.as_str().to_string(),
        fact_count: u64::try_from(batch.fact_count).unwrap_or(0),
        batch_hash: batch.batch_hash.clone(),
        sealed_at: batch.sealed_at,
        acknowledged_at: batch.acknowledged_at,
    }
}

async fn load_local_nodes(
    state: &ServerState,
    repository_id: &RepositoryId,
) -> Result<Vec<LocalNode>, CodypendentError> {
    let rows = sqlx::query(
        "SELECT id, symbol_key, kind, language, package, qualified_name, signature_hash, revision \
         FROM code_nodes WHERE repository = ? ORDER BY symbol_key ASC LIMIT ?",
    )
    .bind(repository_id.to_string())
    .bind(MAX_PUBLICATION_FACTS)
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?;

    let mut nodes = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row.get("id");
        let Ok(id) = CodeNodeId::from_str(&id) else {
            continue;
        };
        let qualified_name: String = row.get("qualified_name");
        nodes.push(LocalNode {
            id,
            symbol_key: row.get("symbol_key"),
            kind: row.get("kind"),
            language: row.get("language"),
            package: row.get("package"),
            qualified_name: Some(qualified_name).filter(|name| !name.is_empty()),
            signature_hash: row.get("signature_hash"),
            revision: row.get("revision"),
        });
    }
    Ok(nodes)
}

struct LocalEdge {
    from: CodeNodeId,
    to: CodeNodeId,
    relation: String,
    confidence: f64,
    evidence_kind: String,
    evidence_artifact: Option<String>,
    revision: String,
}

async fn load_local_edges(
    state: &ServerState,
    repository_id: &RepositoryId,
) -> Result<Vec<LocalEdge>, CodypendentError> {
    let rows = sqlx::query(
        "SELECT e.from_node, e.to_node, e.relation, e.confidence, e.evidence_kind, \
                e.evidence_artifact, e.revision \
         FROM code_edges e \
         JOIN code_nodes f ON f.id = e.from_node \
         JOIN code_nodes t ON t.id = e.to_node \
         WHERE f.repository = ? AND t.repository = ? \
         ORDER BY e.id ASC LIMIT ?",
    )
    .bind(repository_id.to_string())
    .bind(repository_id.to_string())
    .bind(MAX_PUBLICATION_FACTS)
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?;

    let mut edges = Vec::with_capacity(rows.len());
    for row in rows {
        let from: String = row.get("from_node");
        let to: String = row.get("to_node");
        let (Ok(from), Ok(to)) = (CodeNodeId::from_str(&from), CodeNodeId::from_str(&to)) else {
            continue;
        };
        // Migration 0047 constrains confidence to [0.0, 1.0]; a local row
        // outside that range would fail the CHECK, so clamp rather than abort a
        // whole publication on one malformed edge.
        let confidence: f64 = row.get::<f64, _>("confidence").clamp(0.0, 1.0);
        edges.push(LocalEdge {
            from,
            to,
            relation: row.get("relation"),
            confidence,
            evidence_kind: row.get("evidence_kind"),
            evidence_artifact: row.get("evidence_artifact"),
            revision: row.get("revision"),
        });
    }
    Ok(edges)
}

// ---------------------------------------------------------------------------
// Tombstones
// ---------------------------------------------------------------------------

async fn tombstone_facts(
    state: &ServerState,
    principal: PeerPrincipal,
    command_id: CommandId,
    repository: &str,
    subject_kind: &str,
    subject_id: &str,
    reason: &str,
) -> Result<Payload, CodypendentError> {
    let resolved = resolved(state, principal, repository, true).await?;
    let kind = codypendent_federation::tombstone::TombstoneSubjectKind::parse(subject_kind);
    let reason = codypendent_federation::tombstone::TombstoneReason::parse(reason);
    let repository_id = resolved.repository_id.to_string();

    // The subject must belong to THIS repository. Answered with the same
    // not-found a subject that does not exist gets, so a tombstone request
    // cannot be used to probe another checkout's shared ids.
    let published_class = subject_class(state, &repository_id, kind, subject_id).await?;

    let tombstone = match reason {
        codypendent_federation::tombstone::TombstoneReason::Revoked => {
            // An explicit retraction: record the tombstone, mark the publication
            // record `retracted`, and delete the projection row in one
            // transaction.
            TombstoneManager::revoke_publication(
                &state.pool,
                &repository_id,
                kind,
                subject_id,
                i64::from(principal.uid()),
            )
            .await
            .map_err(internal)?
        }
        other => TombstoneManager::record_tombstone(
            &state.pool,
            &repository_id,
            kind,
            subject_id,
            other,
            published_class,
            i64::from(principal.uid()),
        )
        .await
        .map_err(internal)?,
    };
    enqueue_graph_tombstones(state, &repository_id).await?;

    Ok(Payload::GraphTombstoned {
        command_id,
        tombstone_id: tombstone.id,
    })
}

/// The class a subject was published at, or [`repository_not_found`] when it is
/// not a published subject of this repository.
async fn subject_class(
    state: &ServerState,
    repository_id: &str,
    kind: codypendent_federation::tombstone::TombstoneSubjectKind,
    subject_id: &str,
) -> Result<PublicationClass, CodypendentError> {
    use codypendent_federation::tombstone::TombstoneSubjectKind as Kind;
    let class: Option<String> = match kind {
        Kind::Node => sqlx::query_scalar(
            "SELECT class FROM shared_graph_node WHERE shared_node_id = ? AND repository_id = ?",
        )
        .bind(subject_id)
        .bind(repository_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(internal)?,
        Kind::Edge => sqlx::query_scalar(
            "SELECT class FROM shared_graph_edge WHERE shared_edge_id = ? \
             AND (from_repository_id = ? OR to_repository_id = ?)",
        )
        .bind(subject_id)
        .bind(repository_id)
        .bind(repository_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(internal)?,
        // A whole-repository retraction names the repository itself; ownership
        // of it was already resolved by the gate.
        Kind::Repository => {
            if subject_id == repository_id {
                Some(PublicationClass::MetadataShared.as_str().to_string())
            } else {
                None
            }
        }
    };
    class
        .map(|class| PublicationClass::from_str_lenient(&class))
        .ok_or_else(repository_not_found)
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

async fn query_graph(
    state: &ServerState,
    principal: PeerPrincipal,
    command_id: CommandId,
    query: &wire::FederatedGraphQuery,
) -> Result<Payload, CodypendentError> {
    let grants = grants_for_principal(state, principal)
        .await
        .map_err(internal)?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_PAGE_LIMIT)
        .clamp(1, MAX_PAGE_LIMIT);

    let query_hash = stable_query_hash(query);
    let after = match query.cursor.as_ref() {
        Some(cursor) => Some(
            FederationPageCursor::decode_and_verify(
                &cursor.0,
                i64::from(principal.uid()),
                &query_hash,
            )
            .map_err(|_| invalid("pagination cursor does not belong to this query"))?,
        ),
        None => None,
    };

    let store = store(state);
    let mut nodes = Vec::new();
    let mut has_more = false;
    let mut last_id = String::new();

    let rows = sqlx::query(
        "SELECT shared_node_id FROM shared_graph_node \
         WHERE (? IS NULL OR shared_node_id = ?) \
           AND (? IS NULL OR repository_id = ?) \
           AND (? IS NULL OR kind = ?) \
           AND (? IS NULL OR language = ?) \
           AND (? IS NULL OR qualified_name = ?) \
           AND (? IS NULL OR shared_node_id > ?) \
         ORDER BY shared_node_id ASC LIMIT ?",
    )
    .bind(query.node_id.as_deref())
    .bind(query.node_id.as_deref())
    .bind(query.repository_id.as_deref())
    .bind(query.repository_id.as_deref())
    .bind(query.kind.as_deref())
    .bind(query.kind.as_deref())
    .bind(query.language.as_deref())
    .bind(query.language.as_deref())
    .bind(query.symbol_name.as_deref())
    .bind(query.symbol_name.as_deref())
    .bind(after.as_deref())
    .bind(after.as_deref())
    .bind(i64::from(limit) + 1)
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?;

    for row in rows {
        if nodes.len() >= limit as usize {
            has_more = true;
            break;
        }
        let shared_node_id: String = row.get("shared_node_id");
        let Some(node) = store
            .get_shared_node(&shared_node_id)
            .await
            .map_err(internal)?
        else {
            continue;
        };
        // The grant check is applied to the ROW, after it is fetched — never as
        // a filter on the list only. An unauthorized node is skipped exactly as
        // an absent one is.
        if !grants.is_node_authorized(
            &node.repository_id.to_string(),
            node.class,
            node.classification,
        ) {
            continue;
        }
        if let Some(ceiling) = query.class_ceiling {
            if node.class.breadth() > ceiling.breadth() {
                continue;
            }
        }
        last_id = node.shared_node_id.clone();
        nodes.push(node);
    }

    // Edges are returned only when BOTH endpoints are in the page and both
    // repositories are granted, so an edge can never imply the existence of a
    // node the caller may not see.
    let visible: HashSet<String> = nodes.iter().map(|n| n.shared_node_id.clone()).collect();
    let mut edges = Vec::new();
    for node in &nodes {
        let rows = sqlx::query(
            "SELECT shared_edge_id FROM shared_graph_edge WHERE from_shared_node_id = ?",
        )
        .bind(&node.shared_node_id)
        .fetch_all(&state.pool)
        .await
        .map_err(internal)?;
        for row in rows {
            let shared_edge_id: String = row.get("shared_edge_id");
            let Some(edge) = store
                .get_shared_edge(&shared_edge_id)
                .await
                .map_err(internal)?
            else {
                continue;
            };
            if !visible.contains(&edge.to_shared_node_id) {
                continue;
            }
            if !grants.is_edge_authorized(
                &edge.from_repository_id.to_string(),
                &edge.to_repository_id.to_string(),
                edge.class,
                edge.classification,
            ) {
                continue;
            }
            edges.push(edge);
        }
    }

    let cursor = if has_more && !last_id.is_empty() {
        Some(codypendent_protocol::PageCursor(
            FederationPageCursor::encode_cursor(i64::from(principal.uid()), &query_hash, &last_id),
        ))
    } else {
        None
    };

    Ok(Payload::FederatedGraphResult {
        command_id,
        page: Box::new(wire::FederatedGraphPage {
            nodes: nodes.iter().map(node_view).collect(),
            edges: edges.iter().map(edge_view).collect(),
            cursor,
            has_more,
        }),
    })
}

/// A digest over everything in the query EXCEPT the cursor, so a cursor minted
/// for one filter cannot be replayed against another (and, combined with the
/// principal binding inside [`FederationPageCursor`], not by another user).
fn stable_query_hash(query: &wire::FederatedGraphQuery) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    for part in [
        query.node_id.as_deref(),
        query.symbol_name.as_deref(),
        query.kind.as_deref(),
        query.language.as_deref(),
        query.repository_id.as_deref(),
    ] {
        hasher.update(part.unwrap_or("").as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(
        query
            .class_ceiling
            .map_or("", PublicationClass::as_str)
            .as_bytes(),
    );
    hex::encode(hasher.finalize())
}

fn node_view(node: &codypendent_federation::PublishedNode) -> wire::SharedNodeView {
    wire::SharedNodeView {
        shared_node_id: node.shared_node_id.clone(),
        repository_id: node.repository_id.to_string(),
        kind: node.kind.clone(),
        language: node.language.clone(),
        package: node.package.clone(),
        qualified_name: node.qualified_name.clone(),
        source_path: node.source_path.clone(),
        signature_hash: node.signature_hash.clone(),
        class: node.class,
        classification: node.classification,
        revision: node.revision.clone(),
    }
}

fn edge_view(edge: &codypendent_federation::PublishedEdge) -> wire::SharedEdgeView {
    wire::SharedEdgeView {
        shared_edge_id: edge.shared_edge_id.clone(),
        from_shared_node_id: edge.from_shared_node_id.clone(),
        to_shared_node_id: edge.to_shared_node_id.clone(),
        from_repository_id: edge.from_repository_id.to_string(),
        to_repository_id: edge.to_repository_id.to_string(),
        relation: edge.relation.clone(),
        confidence: edge.confidence,
        evidence_kind: edge.evidence_kind.clone(),
        evidence_artifact: edge.evidence_artifact.clone(),
        class: edge.class,
        classification: edge.classification,
        revision: edge.revision.clone(),
    }
}

// ---------------------------------------------------------------------------
// Blast radius / migration plan / reviewers
// ---------------------------------------------------------------------------

/// Resolve a client-named symbol to a `shared_node_id` in `identity`'s
/// repository.
///
/// Accepts a 64-hex `shared_node_id` directly, otherwise looks the symbol up in
/// `code_nodes` (by `symbol_key` first, then `qualified_name`) and derives the
/// shared id from the FEDERATED identity — never from the local repository id,
/// which differs per checkout.
async fn resolve_shared_node(
    state: &ServerState,
    identity: &FederatedRepositoryIdentity,
    node_id: Option<&str>,
    symbol: Option<&str>,
) -> Result<String, CodypendentError> {
    if let Some(node_id) = node_id.map(str::trim).filter(|id| !id.is_empty()) {
        if node_id.len() == 64 && node_id.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Ok(node_id.to_ascii_lowercase());
        }
        // A local `code_nodes.id`: resolve it to its symbol key, scoped to this
        // repository so an id from another checkout is indistinguishable from
        // one that does not exist.
        let symbol_key: Option<String> =
            sqlx::query_scalar("SELECT symbol_key FROM code_nodes WHERE id = ? AND repository = ?")
                .bind(node_id)
                .bind(identity.repository_id.to_string())
                .fetch_optional(&state.pool)
                .await
                .map_err(internal)?;
        let symbol_key = symbol_key.ok_or_else(crate::codegraph::node_not_found)?;
        return Ok(codypendent_federation::derive_shared_node_id(
            &identity.federated_id,
            &symbol_key,
        ));
    }

    let symbol = symbol
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("name a node_id or a symbol"))?;
    let symbol_key: Option<String> = sqlx::query_scalar(
        "SELECT symbol_key FROM code_nodes \
         WHERE repository = ? AND (symbol_key = ? OR qualified_name = ?) \
         ORDER BY symbol_key ASC LIMIT 1",
    )
    .bind(identity.repository_id.to_string())
    .bind(symbol)
    .bind(symbol)
    .fetch_optional(&state.pool)
    .await
    .map_err(internal)?;
    let symbol_key = symbol_key.ok_or_else(crate::codegraph::node_not_found)?;
    Ok(codypendent_federation::derive_shared_node_id(
        &identity.federated_id,
        &symbol_key,
    ))
}

async fn blast_radius(
    state: &ServerState,
    principal: PeerPrincipal,
    command_id: CommandId,
    query: &wire::BlastRadiusQuery,
) -> Result<Payload, CodypendentError> {
    let resolved = resolved(state, principal, &query.repository, true).await?;
    let identity = resolved.identity.clone().ok_or_else(repository_not_found)?;
    let seed = resolve_shared_node(
        state,
        &identity,
        query.node_id.as_deref(),
        query.symbol_name.as_deref().or(query.package.as_deref()),
    )
    .await?;

    let grants = grants_for_principal(state, principal)
        .await
        .map_err(internal)?;
    let depth = query
        .max_depth
        .map_or(DEFAULT_BLAST_RADIUS_DEPTH, |d| d as usize)
        .clamp(1, MAX_BLAST_RADIUS_DEPTH);

    let result = SharedGraphQuery::new(store(state))
        .blast_radius(&seed, &grants, depth)
        .await
        .map_err(traversal_error)?;

    // The crate returns the reachable set, not per-node depth. Recover it by
    // walking the edges it returned — derived from the authorized result, so a
    // node the traversal refused to transit cannot appear here either.
    let depths = depths_from_edges(&seed, &result.reachable_edges);
    let limit = query
        .limit
        .unwrap_or(DEFAULT_PAGE_LIMIT)
        .clamp(1, MAX_PAGE_LIMIT) as usize;

    let mut affected: Vec<wire::BlastRadiusNode> = result
        .reachable_nodes
        .iter()
        .map(|node| {
            let (depth, path) = depths
                .get(&node.shared_node_id)
                .cloned()
                .unwrap_or((0, Vec::new()));
            wire::BlastRadiusNode {
                shared_node_id: node.shared_node_id.clone(),
                repository_id: node.repository_id.to_string(),
                // A policy that withholds symbol names leaves nothing to show
                // but the package and kind — which is the point of
                // `metadata-shared`, not a bug.
                display_name: node
                    .qualified_name
                    .clone()
                    .or_else(|| node.package.clone())
                    .unwrap_or_else(|| node.kind.clone()),
                kind: node.kind.clone(),
                depth: u32::try_from(depth).unwrap_or(u32::MAX),
                relation_path: path,
                class: node.class,
            }
        })
        .collect();
    affected.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then(a.shared_node_id.cmp(&b.shared_node_id))
    });
    let has_more = affected.len() > limit;
    affected.truncate(limit);

    Ok(Payload::BlastRadiusResult {
        command_id,
        report: Box::new(wire::BlastRadiusReport {
            seed_node_id: result.seed_node.shared_node_id.clone(),
            affected_repositories: result
                .impacted_repositories
                .iter()
                .map(ToString::to_string)
                .collect(),
            affected_nodes: affected,
            edge_count: u64::try_from(result.reachable_edges.len()).unwrap_or(0),
            // The traversal is computed in one pass over an authorized frontier;
            // there is no stable keyset to resume from, so a truncated report
            // says so and the client re-asks with a larger limit.
            cursor: None,
            has_more,
        }),
    })
}

/// Breadth-first depths (and the relation path taken) for each node reachable
/// from `seed` over `edges`.
fn depths_from_edges(
    seed: &str,
    edges: &[codypendent_federation::PublishedEdge],
) -> HashMap<String, (usize, Vec<String>)> {
    let mut outgoing: HashMap<&str, Vec<&codypendent_federation::PublishedEdge>> = HashMap::new();
    for edge in edges {
        outgoing
            .entry(edge.from_shared_node_id.as_str())
            .or_default()
            .push(edge);
    }
    let mut depths: HashMap<String, (usize, Vec<String>)> = HashMap::new();
    depths.insert(seed.to_string(), (0, Vec::new()));
    let mut queue = VecDeque::from([seed.to_string()]);
    while let Some(current) = queue.pop_front() {
        let (depth, path) = depths.get(&current).cloned().unwrap_or((0, Vec::new()));
        for edge in outgoing.get(current.as_str()).into_iter().flatten() {
            if depths.contains_key(&edge.to_shared_node_id) {
                continue;
            }
            let mut next_path = path.clone();
            next_path.push(edge.relation.clone());
            depths.insert(edge.to_shared_node_id.clone(), (depth + 1, next_path));
            queue.push_back(edge.to_shared_node_id.clone());
        }
    }
    depths
}

/// A seed that is absent and one the principal's grants do not authorize answer
/// with the identical `graph.node-not-found`, exactly as the crate's traversal
/// invariant requires.
fn traversal_error(error: codypendent_federation::FederationError) -> CodypendentError {
    match error {
        codypendent_federation::FederationError::NodeNotFound(_) => {
            crate::codegraph::node_not_found()
        }
        codypendent_federation::FederationError::InvalidCursor => {
            invalid("pagination cursor does not belong to this query")
        }
        other => internal(other),
    }
}

async fn plan_migration(
    state: &ServerState,
    principal: PeerPrincipal,
    command_id: CommandId,
    query: &wire::MigrationPlanQuery,
) -> Result<Payload, CodypendentError> {
    let resolved = resolved(state, principal, &query.source_repository, true).await?;
    let identity = resolved.identity.clone().ok_or_else(repository_not_found)?;
    let target = resolve_shared_node(state, &identity, None, Some(&query.source_symbol)).await?;

    let grants = grants_for_principal(state, principal)
        .await
        .map_err(internal)?;
    let plan = SharedGraphQuery::new(store(state))
        .migration_plan(&target, &grants)
        .await
        .map_err(traversal_error)?;

    // A client may narrow the plan to specific repositories; it can never widen
    // it, because `impacted_repositories` is already grant-filtered.
    let wanted: HashSet<&str> = query
        .target_repositories
        .iter()
        .map(String::as_str)
        .collect();

    let mut by_repository: HashMap<String, Vec<String>> = HashMap::new();
    for node in &plan.referencing_nodes {
        let repository = node.repository_id.to_string();
        if !wanted.is_empty() && !wanted.contains(repository.as_str()) {
            continue;
        }
        by_repository.entry(repository).or_default().push(
            node.qualified_name.clone().unwrap_or_else(|| {
                // Symbol names withheld by policy: the shared id is the only
                // legitimate handle, and it is what a campaign would target.
                node.shared_node_id.clone()
            }),
        );
    }

    let mut repositories: Vec<String> = by_repository.keys().cloned().collect();
    repositories.sort();
    let steps: Vec<wire::MigrationPlanStep> = repositories
        .iter()
        .enumerate()
        .map(|(index, repository)| {
            let mut symbols = by_repository.get(repository).cloned().unwrap_or_default();
            symbols.sort();
            symbols.dedup();
            wire::MigrationPlanStep {
                step_number: u32::try_from(index + 1).unwrap_or(u32::MAX),
                repository_id: repository.clone(),
                action: match query.target_symbol.as_deref() {
                    Some(target) => format!("migrate references to {target}"),
                    None => format!("update references to {}", query.source_symbol),
                },
                // Risk is the count of referencing symbols, bucketed. It is the
                // only evidence the published graph carries — deliberately not
                // dressed up as anything more.
                estimated_risk: match symbols.len() {
                    0..=2 => "low",
                    3..=10 => "medium",
                    _ => "high",
                }
                .to_string(),
                target_symbols: symbols,
            }
        })
        .collect();

    Ok(Payload::MigrationPlanResult {
        command_id,
        report: Box::new(wire::MigrationPlanReport {
            title: format!("{}: {}", query.kind.as_str(), query.source_symbol),
            kind: query.kind,
            source_repository: resolved.repository_id.to_string(),
            total_affected_repositories: u32::try_from(steps.len()).unwrap_or(u32::MAX),
            steps,
        }),
    })
}

async fn suggest_reviewers(
    state: &ServerState,
    principal: PeerPrincipal,
    command_id: CommandId,
    query: &wire::ReviewerSuggestionQuery,
) -> Result<Payload, CodypendentError> {
    let resolved = resolved(state, principal, &query.repository, true).await?;
    let grants = grants_for_principal(state, principal)
        .await
        .map_err(internal)?;
    let suggestions = SharedGraphQuery::new(store(state))
        .reviewer_suggestions(&resolved.repository_id, &grants)
        .await
        .map_err(traversal_error)?;

    let limit = query
        .limit
        .unwrap_or(DEFAULT_PAGE_LIMIT)
        .clamp(1, MAX_PAGE_LIMIT) as usize;
    let suggestions: Vec<wire::ReviewerSuggestion> = suggestions
        .into_iter()
        .take(limit)
        .map(|suggestion| wire::ReviewerSuggestion {
            reviewer_id: format!("uid:{}", suggestion.principal_uid),
            // Not a model score. The single fact behind this suggestion — that
            // this uid established the repository's federated identity — is
            // either true or it is not, so it is reported at 1.0 rather than
            // given a fabricated gradient.
            confidence: 1.0,
            reason: suggestion.rationale,
            relevant_symbols: query.changed_symbols.clone(),
            relevant_repositories: vec![suggestion.repository_id.to_string()],
        })
        .collect();

    Ok(Payload::ReviewerSuggestionsResult {
        command_id,
        suggestions: Box::new(wire::ReviewerSuggestions { suggestions }),
    })
}

// ---------------------------------------------------------------------------
// Campaigns
// ---------------------------------------------------------------------------

async fn create_campaign(
    state: &ServerState,
    principal: PeerPrincipal,
    command_id: CommandId,
    request: &wire::CreateCampaignRequest,
) -> Result<Payload, CodypendentError> {
    let kind = match request.kind {
        wire::CampaignKind::ApiMigration => FedCampaignKind::ApiMigration,
        wire::CampaignKind::SchemaMigration => FedCampaignKind::SchemaMigration,
        wire::CampaignKind::DependencyUpgrade => FedCampaignKind::DependencyUpgrade,
        wire::CampaignKind::OwnershipReview => FedCampaignKind::OwnershipReview,
        wire::CampaignKind::Custom => FedCampaignKind::Custom,
        // A kind a newer client defined. Refused structurally at the edge rather
        // than silently coerced into `custom`, which would run a campaign the
        // client did not ask for.
        _ => {
            return Err(invalid(
                "this daemon does not understand that campaign kind",
            ))
        }
    };
    if request.repositories.is_empty() {
        return Err(invalid("a campaign must enrol at least one repository"));
    }
    if request.idempotency_key.trim().is_empty() {
        return Err(invalid("a campaign requires an idempotency key"));
    }

    let mut targets = Vec::with_capacity(request.repositories.len());
    for enrolment in &request.repositories {
        let resolved = resolved(state, principal, &enrolment.repository, true).await?;
        let identity = resolved.identity.clone().ok_or_else(repository_not_found)?;
        targets.push(TargetRepositorySpec {
            repository_id: resolved.repository_id.to_string(),
            // The enrolment snapshots the federated id so a later identity
            // change cannot silently retarget an in-flight campaign.
            federated_id: identity.federated_id,
            // Per-repository worktree, never shared: a shared worktree would let
            // a denial in repository A be bypassed by an approved write in B.
            worktree_path: enrolment
                .worktree_path
                .clone()
                .or_else(|| Some(resolved.root.to_string_lossy().into_owned())),
            budget_minor_units: enrolment
                .budget_minor_units
                .and_then(|budget| i64::try_from(budget).ok()),
            approval_mode: match enrolment.approval_mode {
                wire::CampaignApprovalMode::PerRun => FedApprovalMode::PerRun,
                // `per-effect` is the strictest mode, so an approval mode this
                // daemon does not recognize lands there rather than on the
                // looser one.
                _ => FedApprovalMode::PerEffect,
            },
        });
    }

    let (campaign, _repositories) = CampaignEngine::create_campaign_idempotent(
        &state.pool,
        i64::from(principal.uid()),
        &request.title,
        kind,
        &request.workflow_id,
        &request.idempotency_key,
        &targets,
    )
    .await
    .map_err(internal)?;

    Ok(Payload::CampaignCreated {
        command_id,
        campaign_id: campaign.id,
    })
}

async fn get_campaign(
    state: &ServerState,
    principal: PeerPrincipal,
    command_id: CommandId,
    campaign_id: &str,
) -> Result<Payload, CodypendentError> {
    let uid = i64::from(principal.uid());
    let campaign = campaign_view(state, uid, campaign_id).await?;

    let repositories = sqlx::query(
        "SELECT repository_id, federated_id, worktree_path, budget_minor_units, approval_mode, \
                state, enrolled_at, terminal_at \
         FROM campaign_repositories WHERE campaign_id = ? ORDER BY enrolled_at ASC",
    )
    .bind(campaign_id)
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?
    .into_iter()
    .map(|row| wire::CampaignRepositoryView {
        campaign_id: campaign_id.to_string(),
        repository_id: row.get("repository_id"),
        federated_id: row.get("federated_id"),
        worktree_path: row.get("worktree_path"),
        budget_minor_units: row
            .get::<Option<i64>, _>("budget_minor_units")
            .and_then(|budget| u64::try_from(budget).ok()),
        approval_mode: match row.get::<String, _>("approval_mode").as_str() {
            "per-run" => wire::CampaignApprovalMode::PerRun,
            _ => wire::CampaignApprovalMode::PerEffect,
        },
        state: repo_state(&row.get::<String, _>("state")),
        enrolled_at: parse_ts(&row.get::<String, _>("enrolled_at")),
        terminal_at: row
            .get::<Option<String>, _>("terminal_at")
            .as_deref()
            .map(parse_ts),
    })
    .collect();

    let mut runs = Vec::new();
    for row in sqlx::query(
        "SELECT repository_id, run_id, attempt, state, created_at, terminal_at \
         FROM campaign_runs WHERE campaign_id = ? ORDER BY created_at ASC",
    )
    .bind(campaign_id)
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?
    {
        let run_id: String = row.get("run_id");
        let Some(run_id) = workflow_run_id_as_run_id(&run_id) else {
            continue;
        };
        runs.push(wire::CampaignRunView {
            campaign_id: campaign_id.to_string(),
            repository_id: row.get("repository_id"),
            run_id,
            attempt: u32::try_from(row.get::<i64, _>("attempt")).unwrap_or(1),
            state: row.get("state"),
            created_at: parse_ts(&row.get::<String, _>("created_at")),
            terminal_at: row
                .get::<Option<String>, _>("terminal_at")
                .as_deref()
                .map(parse_ts),
        });
    }

    let approvals = sqlx::query(
        "SELECT repository_id, approval_id, action_digest, decision, decided_at \
         FROM campaign_approvals WHERE campaign_id = ? ORDER BY approval_id ASC",
    )
    .bind(campaign_id)
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?
    .into_iter()
    .map(|row| wire::CampaignApprovalView {
        campaign_id: campaign_id.to_string(),
        repository_id: row.get("repository_id"),
        approval_id: row.get("approval_id"),
        action_digest: row.get("action_digest"),
        decision: row.get("decision"),
        decided_at: row
            .get::<Option<String>, _>("decided_at")
            .as_deref()
            .map(parse_ts),
    })
    .collect();

    let effects = sqlx::query(
        "SELECT id, repository_id, run_id, effect_kind, effect_digest, applied_at \
         FROM campaign_effects WHERE campaign_id = ? ORDER BY applied_at ASC",
    )
    .bind(campaign_id)
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?
    .into_iter()
    .map(|row| wire::CampaignEffectView {
        id: row.get("id"),
        campaign_id: campaign_id.to_string(),
        repository_id: row.get("repository_id"),
        run_id: row.get("run_id"),
        effect_kind: row.get("effect_kind"),
        effect_digest: row.get("effect_digest"),
        applied_at: parse_ts(&row.get::<String, _>("applied_at")),
    })
    .collect();

    Ok(Payload::CampaignDetail {
        command_id,
        detail: Box::new(wire::CampaignDetailView {
            campaign,
            repositories,
            runs,
            approvals,
            effects,
        }),
    })
}

async fn list_campaigns(
    state: &ServerState,
    principal: PeerPrincipal,
    command_id: CommandId,
    filter: Option<wire::CampaignState>,
    limit: Option<u32>,
) -> Result<Payload, CodypendentError> {
    let limit = limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT);
    let filter = filter.map(|state| state.as_str().to_string());
    let rows = sqlx::query(
        "SELECT id, title, kind, workflow_id, state, repository_count, created_at, updated_at, \
                terminal_at \
         FROM campaigns WHERE owner_uid = ? AND (? IS NULL OR state = ?) \
         ORDER BY updated_at DESC LIMIT ?",
    )
    .bind(i64::from(principal.uid()))
    .bind(filter.as_deref())
    .bind(filter.as_deref())
    .bind(i64::from(limit))
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?;

    Ok(Payload::CampaignList {
        command_id,
        campaigns: rows.iter().map(row_to_campaign_view).collect(),
    })
}

async fn execute_campaign(
    state: &ServerState,
    principal: PeerPrincipal,
    client_id: Option<ClientId>,
    command_id: CommandId,
    request: &wire::ExecuteCampaignRequest,
) -> Result<Payload, CodypendentError> {
    let uid = i64::from(principal.uid());
    let campaign = campaign_view(state, uid, &request.campaign_id).await?;
    let Some(client_id) = client_id else {
        return Err(internal("connection has no client id"));
    };
    // A campaign creates ORDINARY per-repository workflow runs through the same
    // seam `StartWorkflow` uses. It is a coordinator, never an authority: there
    // is no campaign runtime, no shared worktree and no shared budget here.
    let Some(starter) = state.starter.as_ref() else {
        return Err(CodypendentError::new(
            "workflow.transport-unavailable",
            "this daemon cannot start workflow runs, so it cannot execute a campaign",
            false,
        ));
    };
    if matches!(
        campaign.state,
        wire::CampaignState::Cancelled | wire::CampaignState::Completed
    ) {
        return Err(CodypendentError::new(
            "federation.campaign-terminal",
            "this campaign has already reached a terminal state",
            false,
        ));
    }

    // Only failed/denied repositories are re-driven on a retry: a repository
    // that already succeeded is never re-run.
    let wanted_states: &[&str] = if request.retry_failed_only {
        &["failed", "denied"]
    } else {
        &["pending"]
    };
    let rows = sqlx::query(
        "SELECT repository_id, worktree_path, state FROM campaign_repositories \
         WHERE campaign_id = ? ORDER BY enrolled_at ASC",
    )
    .bind(&request.campaign_id)
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?;

    let now = Utc::now().to_rfc3339();
    let mut started = 0usize;
    for row in rows {
        let repository_id: String = row.get("repository_id");
        let repo_state: String = row.get("state");
        if !wanted_states.contains(&repo_state.as_str()) {
            continue;
        }
        let worktree_path: Option<String> = row.get("worktree_path");
        let last_attempt: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(attempt), 0) FROM campaign_runs \
             WHERE campaign_id = ? AND repository_id = ?",
        )
        .bind(&request.campaign_id)
        .bind(&repository_id)
        .fetch_one(&state.pool)
        .await
        .map_err(internal)?;
        let attempt = last_attempt + 1;
        // The exact key shape `CampaignEngine` uses, so a crashed coordinator's
        // retry adopts the existing run instead of forking a second one.
        let idempotency_key = format!(
            "campaign:{}:repo:{repository_id}:attempt:{attempt}",
            request.campaign_id
        );

        let run_id = starter
            .start(StartWorkflowRequest {
                expected_definition_digest: None,
                manifest: String::new(),
                workflow_id: Some(campaign.workflow_id.clone()),
                inputs: serde_json::json!({
                    "campaign_id": request.campaign_id,
                    "repository_id": repository_id,
                }),
                idempotency_key: idempotency_key.clone(),
                repository: worktree_path,
                // Kernel-derived, stamped here: the coordinator's authority
                // never exceeds this uid's.
                owner_uid: principal.uid(),
                client_id,
                // A campaign carries no per-run ceiling of its own; the
                // campaign workflow's manifest envelope applies.
                budget_ceiling: None,
            })
            .await?;

        sqlx::query(
            // `INSERT OR IGNORE` rather than a targeted `ON CONFLICT`: the table
            // has two ways to collide — the (campaign, repository, attempt)
            // primary key and the UNIQUE on `run_id` — and an adopted
            // idempotent run legitimately hits the second. Either way the row
            // already records this attempt.
            "INSERT OR IGNORE INTO campaign_runs \
             (campaign_id, repository_id, run_id, attempt, idempotency_key, state, created_at, terminal_at) \
             VALUES (?, ?, ?, ?, ?, 'running', ?, NULL)",
        )
        .bind(&request.campaign_id)
        .bind(&repository_id)
        .bind(&run_id)
        .bind(attempt)
        .bind(&idempotency_key)
        .bind(&now)
        .execute(&state.pool)
        .await
        .map_err(internal)?;

        sqlx::query(
            "UPDATE campaign_repositories SET state = 'running', terminal_at = NULL \
             WHERE campaign_id = ? AND repository_id = ?",
        )
        .bind(&request.campaign_id)
        .bind(&repository_id)
        .execute(&state.pool)
        .await
        .map_err(internal)?;
        started += 1;
    }

    if started > 0 {
        sqlx::query(
            "UPDATE campaigns SET state = 'running', terminal_at = NULL, updated_at = ? WHERE id = ?",
        )
        .bind(&now)
        .bind(&request.campaign_id)
        .execute(&state.pool)
        .await
        .map_err(internal)?;
    }

    Ok(Payload::CampaignExecuted {
        command_id,
        campaign: Box::new(campaign_view(state, uid, &request.campaign_id).await?),
    })
}

async fn cancel_campaign(
    state: &ServerState,
    command_id: CommandId,
    campaign_id: &str,
) -> Result<Payload, CodypendentError> {
    // Ownership was resolved by the gate; a campaign row is reachable here only
    // for its owner.
    let now = Utc::now().to_rfc3339();
    let mut tx = state.pool.begin().await.map_err(internal)?;
    sqlx::query(
        "UPDATE campaign_repositories SET state = 'skipped', terminal_at = ? \
         WHERE campaign_id = ? AND state IN ('pending', 'running')",
    )
    .bind(&now)
    .bind(campaign_id)
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    sqlx::query(
        "UPDATE campaigns SET state = 'cancelled', terminal_at = ?, updated_at = ? \
         WHERE id = ? AND state NOT IN ('completed', 'cancelled')",
    )
    .bind(&now)
    .bind(&now)
    .bind(campaign_id)
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;

    Ok(Payload::CampaignCancelled {
        command_id,
        campaign_id: campaign_id.to_string(),
    })
}

async fn campaign_view(
    state: &ServerState,
    owner_uid: i64,
    campaign_id: &str,
) -> Result<wire::CampaignView, CodypendentError> {
    let row = sqlx::query(
        "SELECT id, title, kind, workflow_id, state, repository_count, created_at, updated_at, \
                terminal_at \
         FROM campaigns WHERE id = ? AND owner_uid = ?",
    )
    .bind(campaign_id)
    .bind(owner_uid)
    .fetch_optional(&state.pool)
    .await
    .map_err(internal)?
    .ok_or_else(campaign_not_found)?;
    Ok(row_to_campaign_view(&row))
}

fn row_to_campaign_view(row: &sqlx::sqlite::SqliteRow) -> wire::CampaignView {
    wire::CampaignView {
        id: row.get("id"),
        title: row.get("title"),
        kind: wire::CampaignKind::from_str_lenient(&row.get::<String, _>("kind")),
        workflow_id: row.get("workflow_id"),
        state: wire::CampaignState::from_str_lenient(&row.get::<String, _>("state")),
        repository_count: u32::try_from(row.get::<i64, _>("repository_count")).unwrap_or(0),
        created_at: parse_ts(&row.get::<String, _>("created_at")),
        updated_at: parse_ts(&row.get::<String, _>("updated_at")),
        terminal_at: row
            .get::<Option<String>, _>("terminal_at")
            .as_deref()
            .map(parse_ts),
    }
}

fn repo_state(raw: &str) -> wire::CampaignRepoState {
    match raw {
        "pending" => wire::CampaignRepoState::Pending,
        "running" => wire::CampaignRepoState::Running,
        "succeeded" => wire::CampaignRepoState::Succeeded,
        "failed" => wire::CampaignRepoState::Failed,
        "denied" => wire::CampaignRepoState::Denied,
        "skipped" => wire::CampaignRepoState::Skipped,
        _ => wire::CampaignRepoState::Unknown,
    }
}

fn parse_ts(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw).map_or_else(|_| Utc::now(), |ts| ts.with_timezone(&Utc))
}

/// A durable workflow run id is `wfrun-<32 hex>` (`deterministic_run_id`), and
/// `CampaignRunView.run_id` is a `RunId` (a UUID newtype). Those 32 hex digits
/// are exactly 16 bytes, so the mapping is lossless in both directions — a
/// client reconstructs the workflow id by prefixing `wfrun-` to the simple form.
fn workflow_run_id_as_run_id(raw: &str) -> Option<codypendent_protocol::RunId> {
    let hex = raw.strip_prefix("wfrun-").unwrap_or(raw);
    codypendent_protocol::RunId::from_str(hex).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::DataClassification;

    /// The two federation refusals must be indistinguishable from "not there":
    /// no path, no id, nothing a caller could use to confirm that a repository
    /// or campaign exists under another principal.
    #[test]
    fn federation_refusals_leak_nothing() {
        for error in [repository_not_found(), campaign_not_found()] {
            assert!(!error.retryable);
            assert!(
                !error.message.contains('/'),
                "a refusal must not echo a path: {}",
                error.message
            );
            assert!(
                !error.message.chars().any(|c| c.is_ascii_digit()),
                "a refusal must not echo an id: {}",
                error.message
            );
        }
    }

    /// Every command this module serves must be classified either as a read
    /// (any attached role) or as a mutation (Controller). A body that is
    /// neither would silently take the mutation floor by accident.
    #[test]
    fn every_federation_command_is_classified() {
        let bodies = sample_bodies();
        for body in &bodies {
            assert!(
                is_federation_command(body),
                "{body:?} is not routed to the federation handler"
            );
        }
        // Reads are a strict subset of the routed set.
        for body in &bodies {
            if is_federation_read(body) {
                assert!(is_federation_command(body));
            }
        }
    }

    /// Breadth-first depth recovery must agree with the edge set the traversal
    /// returned, including the relation path taken to reach each node.
    #[test]
    fn depths_are_recovered_from_the_authorized_edge_set() {
        let edge = |from: &str, to: &str, relation: &str| codypendent_federation::PublishedEdge {
            shared_edge_id: format!("{from}-{to}"),
            from_shared_node_id: from.to_string(),
            to_shared_node_id: to.to_string(),
            from_repository_id: RepositoryId::new(),
            to_repository_id: RepositoryId::new(),
            relation: relation.to_string(),
            confidence: 1.0,
            evidence_kind: "syntax".to_string(),
            evidence_artifact: None,
            class: PublicationClass::MetadataShared,
            classification: DataClassification::Internal,
            class_inputs_digest: "0".repeat(64),
            revision: "rev".to_string(),
            content_hash: "0".repeat(64),
            computed_at: Utc::now(),
        };
        let edges = vec![edge("a", "b", "calls"), edge("b", "c", "imports")];
        let depths = depths_from_edges("a", &edges);
        assert_eq!(depths.get("a").map(|(d, _)| *d), Some(0));
        assert_eq!(depths.get("b").map(|(d, _)| *d), Some(1));
        assert_eq!(depths.get("c").map(|(d, _)| *d), Some(2));
        assert_eq!(
            depths.get("c").map(|(_, path)| path.clone()),
            Some(vec!["calls".to_string(), "imports".to_string()])
        );
    }

    /// A workflow run id must survive the trip through `RunId` unchanged.
    #[test]
    fn workflow_run_ids_round_trip_through_run_id() {
        let raw = format!("wfrun-{}", "ab".repeat(16));
        let run_id = workflow_run_id_as_run_id(&raw).expect("parse");
        assert_eq!(format!("wfrun-{}", run_id.0.simple()), raw);
    }

    pub(super) fn sample_bodies() -> Vec<CommandBody> {
        vec![
            CommandBody::EstablishFederatedIdentity {
                repository: "/repo".to_string(),
                display_name: None,
            },
            CommandBody::GetPublicationPolicy {
                repository: "/repo".to_string(),
            },
            CommandBody::SetPublicationPolicy {
                repository: "/repo".to_string(),
                policy: wire::UpdatePublicationPolicyRequest::default(),
            },
            CommandBody::PublishGraphFacts {
                repository: "/repo".to_string(),
                idempotency_key: "key".to_string(),
            },
            CommandBody::TombstoneGraphFacts {
                repository: "/repo".to_string(),
                subject_kind: "node".to_string(),
                subject_id: "0".repeat(64),
                reason: "revoked".to_string(),
            },
            CommandBody::QueryFederatedGraph {
                query: wire::FederatedGraphQuery::default(),
            },
            CommandBody::QueryBlastRadius {
                query: wire::BlastRadiusQuery::default(),
            },
            CommandBody::PlanMigration {
                query: wire::MigrationPlanQuery {
                    source_repository: "/repo".to_string(),
                    source_symbol: "sym".to_string(),
                    target_symbol: None,
                    target_repositories: Vec::new(),
                    kind: wire::CampaignKind::ApiMigration,
                },
            },
            CommandBody::SuggestReviewers {
                query: wire::ReviewerSuggestionQuery::default(),
            },
            CommandBody::CreateCampaign {
                campaign: wire::CreateCampaignRequest {
                    title: "t".to_string(),
                    kind: wire::CampaignKind::Custom,
                    workflow_id: "w".to_string(),
                    repositories: Vec::new(),
                    idempotency_key: "key".to_string(),
                },
            },
            CommandBody::GetCampaign {
                campaign_id: "c".to_string(),
            },
            CommandBody::ListCampaigns {
                state: None,
                limit: None,
            },
            CommandBody::ExecuteCampaign {
                request: wire::ExecuteCampaignRequest {
                    campaign_id: "c".to_string(),
                    retry_failed_only: false,
                },
            },
            CommandBody::CancelCampaign {
                campaign_id: "c".to_string(),
            },
        ]
    }
}
