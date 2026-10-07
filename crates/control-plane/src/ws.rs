use axum::{
    extract::{
        ws::{rejection::WebSocketUpgradeRejection, Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    auth::{AuthPrincipal, Principal},
    authz::{authorize_organization_action, authorize_repository_action, Action},
    error::ControlPlaneError,
    state::AppState,
};

const REPLAY_PAGE_SIZE: usize = 100;
const MAX_REPLAY_EVENTS_PER_CONNECTION: usize = 10_000;

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    /// Legacy bearer-token parameter. Refused: long-lived credentials must
    /// never enter request URLs or intermediary logs.
    pub token: Option<String>,
    /// A 30-second, single-use, repository-scoped upgrade ticket.
    pub ticket: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WsTicketRequest {
    pub organization_id: Uuid,
    pub repository_id: Option<Uuid>,
    pub stream: Option<String>,
    pub last_event_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct WsTicketResponse {
    pub ticket: String,
    pub expires_at: DateTime<Utc>,
}

/// Mint a narrowly scoped browser-WebSocket ticket using a normal authenticated
/// HTTP request. Browsers cannot attach an `Authorization` header to the native
/// `WebSocket` constructor, so the socket consumes this opaque one-time grant
/// instead of putting the caller's long-lived bearer token in the URL.
pub async fn issue_ws_ticket(
    State(state): State<AppState>,
    AuthPrincipal(principal): AuthPrincipal,
    Json(request): Json<WsTicketRequest>,
) -> Result<Json<WsTicketResponse>, ControlPlaneError> {
    if let Some(repository_id) = request.repository_id {
        authorize_repository_action(
            state.store.as_ref(),
            &principal,
            request.organization_id,
            repository_id,
            Action::Read,
        )
        .await?;
    } else {
        // An organization-wide subscription is only available to a caller with
        // an organization-wide grant; repository-only grants cannot be widened
        // by omitting the repository id.
        authorize_organization_action(
            state.store.as_ref(),
            &principal,
            request.organization_id,
            Action::Read,
        )
        .await?;
    }

    let stream = request.stream.unwrap_or_else(|| "sync".to_string());
    if stream.is_empty()
        || stream.len() > 64
        || !stream
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ControlPlaneError::BadRequest(
            "invalid event stream name".to_string(),
        ));
    }
    let last_event_id = request.last_event_id.unwrap_or(0);
    if last_event_id < 0 {
        return Err(ControlPlaneError::BadRequest(
            "last_event_id cannot be negative".to_string(),
        ));
    }

    let (ticket, expires_at) = state.issue_ws_ticket(
        principal,
        request.organization_id,
        request.repository_id,
        stream,
        last_event_id,
    )?;

    Ok(Json(WsTicketResponse { ticket, expires_at }))
}

/// `ws` is taken as a `Result` deliberately: `WebSocketUpgrade`'s own rejection
/// runs before this function body, and a malformed handshake must not be able to
/// pre-empt (or mask) the credential and authorization checks below. The upgrade
/// is required, just last. (axum 0.8 reserves `Option<T>` for extractors that
/// implement `OptionalFromRequestParts`, which `WebSocketUpgrade` does not;
/// `Result<T, T::Rejection>` is the spelling that keeps the rejection deferred.)
pub async fn ws_handler(
    State(state): State<AppState>,
    Query(query): Query<WsQuery>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<impl IntoResponse, ControlPlaneError> {
    if query.token.is_some() {
        return Err(ControlPlaneError::BadRequest(
            "bearer credentials are not accepted in the websocket URL; request a websocket ticket"
                .to_string(),
        ));
    }

    let ticket = query.ticket.as_deref().ok_or_else(|| {
        ControlPlaneError::Unauthorized("websocket ticket is required".to_string())
    })?;
    // Removal happens before upgrade acceptance. Even if the ticket later
    // appears in a proxy log, it cannot authorize a second connection.
    let grant = state.consume_ws_ticket(ticket).ok_or_else(|| {
        ControlPlaneError::Unauthorized("invalid or expired websocket ticket".to_string())
    })?;
    let principal = grant.principal;
    let org_id = grant.organization_id;
    let repo_id = grant.repository_id;

    if let Some(repo_id) = repo_id {
        authorize_repository_action(
            state.store.as_ref(),
            &principal,
            org_id,
            repo_id,
            Action::Read,
        )
        .await?;
    } else {
        authorize_organization_action(state.store.as_ref(), &principal, org_id, Action::Read)
            .await?;
    }

    // Only once the subscription is authorized does the handshake itself matter.
    let ws = ws.map_err(|rejection| {
        ControlPlaneError::BadRequest(format!("websocket upgrade required: {rejection}"))
    })?;

    let stream_name = grant.stream;
    let last_id = grant.last_event_id;

    Ok(ws.on_upgrade(move |socket| {
        handle_socket(
            socket,
            state,
            principal,
            org_id,
            repo_id,
            stream_name,
            last_id,
        )
    }))
}

/// Re-evaluates the subscription's authorization. Connections outlive grants:
/// authorizing once at upgrade let a revoked grant keep receiving events for the
/// life of the socket.
async fn still_authorized(
    state: &AppState,
    principal: &Principal,
    org_id: Uuid,
    repo_id: Option<Uuid>,
) -> bool {
    match repo_id {
        Some(repo_id) => authorize_repository_action(
            state.store.as_ref(),
            principal,
            org_id,
            repo_id,
            Action::Read,
        )
        .await
        .is_ok(),
        None => {
            authorize_organization_action(state.store.as_ref(), principal, org_id, Action::Read)
                .await
                .is_ok()
        }
    }
}

async fn handle_socket(
    socket: WebSocket,
    state: AppState,
    principal: Principal,
    org_id: Uuid,
    repo_id: Option<Uuid>,
    stream_name: String,
    last_id: i64,
) {
    let (mut sender, mut receiver) = socket.split();

    // Subscribe before reading the durable log. An event committed between a
    // query and a later subscription would otherwise live in neither result
    // set for this connection. Events present in both are de-duplicated by the
    // durable id after replay.
    let mut rx = state.events_tx.subscribe();
    let mut delivered_through = last_id;
    let mut replayed = 0_usize;

    // 1. Replay historical missed events, scoped to the one authorized
    //    repository. Page until caught up: serving only the first page and then
    //    switching to the broadcast loses every older event after item 100.
    loop {
        if !still_authorized(&state, &principal, org_id, repo_id).await {
            return;
        }
        let historical = match state
            .store
            .query_stream_events(
                org_id,
                repo_id,
                &stream_name,
                delivered_through,
                REPLAY_PAGE_SIZE,
            )
            .await
        {
            Ok(events) => events,
            // Fail closed: a store error is not evidence that delivery is permitted.
            Err(_) => return,
        };
        let page_len = historical.len();
        for event in historical {
            delivered_through = delivered_through.max(event.id);
            let msg_text = match serde_json::to_string(&event) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if sender.send(Message::Text(msg_text.into())).await.is_err() {
                return;
            }
        }
        replayed += page_len;
        if page_len < REPLAY_PAGE_SIZE {
            break;
        }
        if replayed >= MAX_REPLAY_EVENTS_PER_CONNECTION {
            // Bound one connection's catch-up work. Closing is safe: the client
            // reconnects with the last delivered id and resumes the next page.
            return;
        }
    }

    // 2. Consume the live channel captured before replay. Anything replayed
    //    from the durable log may also be queued here, so skip it by id.
    tokio::select! {
        _ = async {
            while let Ok(msg) = rx.recv().await {
                // Events with no repository cannot be proved in scope for this
                // subscription, so they are not delivered.
                if msg.organization_id != org_id
                    || msg.stream != stream_name
                    || repo_id.is_some_and(|repo_id| msg.repository_id != Some(repo_id))
                {
                    continue;
                }
                if msg.id <= delivered_through {
                    continue;
                }

                // Re-authorize per delivery batch so a grant revoked mid-connection
                // stops delivery instead of streaming until the client disconnects.
                if !still_authorized(&state, &principal, org_id, repo_id).await {
                    break;
                }

                if let Ok(json_str) = serde_json::to_string(&msg) {
                    if sender.send(Message::Text(json_str.into())).await.is_err() {
                        break;
                    }
                }
            }
        } => {},
        _ = async {
            while let Some(Ok(msg)) = receiver.next().await {
                if let Message::Close(_) = msg {
                    break;
                }
            }
        } => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::{
        config::ControlPlaneConfig,
        storage::MemoryStorageDriver,
        store::{
            memory::MemoryStore, Membership, Organization, Repository, RoleGrant, Store,
            StreamEvent, User,
        },
    };

    const TEST_JWT_SECRET: &str = "ctrl-plane-unit-test-signing-key-0123456789abcdef";

    #[test]
    fn websocket_tickets_are_opaque_scoped_and_single_use() {
        let config = ControlPlaneConfig::from_env_with_jwt_secret(TEST_JWT_SECRET).unwrap();
        let state = AppState::new(
            config,
            Arc::new(MemoryStore::new()),
            Arc::new(MemoryStorageDriver::new()),
        );
        let org_id = Uuid::now_v7();
        let repo_id = Uuid::now_v7();
        let principal = Principal::User {
            id: Uuid::now_v7(),
            email: None,
            display_name: "Browser".to_string(),
        };
        let (raw, _) = state
            .issue_ws_ticket(
                principal.clone(),
                org_id,
                Some(repo_id),
                "sync".to_string(),
                42,
            )
            .expect("ticket registry");
        assert!(raw.starts_with("cp_ws_"));
        let grant = state.consume_ws_ticket(&raw).expect("first use succeeds");
        assert_eq!(grant.principal, principal);
        assert_eq!(grant.organization_id, org_id);
        assert_eq!(grant.repository_id, Some(repo_id));
        assert_eq!(grant.last_event_id, 42);
        assert!(state.consume_ws_ticket(&raw).is_none(), "ticket is one-use");
    }

    /// A subscription authorized once at upgrade must not keep delivering after
    /// the grant behind it stops being valid. This drives the predicate the live
    /// loop consults before every delivery batch, across the moment the grant
    /// lapses.
    #[tokio::test]
    async fn authorization_is_re_evaluated_after_upgrade() {
        let config = ControlPlaneConfig::from_env_with_jwt_secret(TEST_JWT_SECRET)
            .expect("test signing secret must be accepted");
        let store = Arc::new(MemoryStore::new());
        let state = AppState::new(config, store.clone(), Arc::new(MemoryStorageDriver::new()));

        let org_id = Uuid::now_v7();
        let repo_id = Uuid::now_v7();
        let user_id = Uuid::now_v7();
        let now = chrono::Utc::now();

        store
            .create_organization(Organization {
                id: org_id,
                slug: "acme".to_string(),
                display_name: "Acme".to_string(),
                max_publication_class: "content-shared".to_string(),
                max_classification: "internal".to_string(),
                data_residency: None,
                retention_days: None,
                policy_version: 1,
                created_at: now,
            })
            .await
            .unwrap();

        store
            .create_repository(Repository {
                id: repo_id,
                organization_id: org_id,
                federated_id: "a".repeat(64),
                display_name: "Repo".to_string(),
                max_publication_class: "content-shared".to_string(),
                max_classification: "internal".to_string(),
                policy_version: 1,
                created_at: now,
            })
            .await
            .unwrap();

        store
            .create_user(User {
                id: user_id,
                display_name: "Subscriber".to_string(),
                primary_email: None,
                state: "active".to_string(),
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
        store
            .add_membership(Membership {
                organization_id: org_id,
                user_id,
                state: "active".to_string(),
                joined_at: Some(now),
                created_at: now,
            })
            .await
            .unwrap();

        // A grant that lapses shortly after the connection is established.
        store
            .create_role_grant(RoleGrant {
                id: Uuid::now_v7(),
                organization_id: org_id,
                user_id: Some(user_id),
                team_id: None,
                repository_id: Some(repo_id),
                role: "observer".to_string(),
                action_scope: None,
                granted_by: user_id,
                granted_at: now,
                expires_at: Some(now + chrono::Duration::milliseconds(300)),
                revoked_at: None,
            })
            .await
            .unwrap();

        let principal = Principal::User {
            id: user_id,
            email: None,
            display_name: "Subscriber".to_string(),
        };

        assert!(
            still_authorized(&state, &principal, org_id, Some(repo_id)).await,
            "a live grant must authorize delivery"
        );

        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        assert!(
            !still_authorized(&state, &principal, org_id, Some(repo_id)).await,
            "delivery must stop once the grant is no longer valid"
        );
    }

    /// A principal with no grant at all is refused, so the predicate cannot be
    /// satisfied merely by holding an open socket.
    #[tokio::test]
    async fn a_principal_without_a_grant_is_never_authorized() {
        let config = ControlPlaneConfig::from_env_with_jwt_secret(TEST_JWT_SECRET)
            .expect("test signing secret must be accepted");
        let store = Arc::new(MemoryStore::new());
        let state = AppState::new(config, store.clone(), Arc::new(MemoryStorageDriver::new()));

        let principal = Principal::User {
            id: Uuid::now_v7(),
            email: None,
            display_name: "Stranger".to_string(),
        };

        assert!(!still_authorized(&state, &principal, Uuid::now_v7(), Some(Uuid::now_v7())).await);
    }

    #[tokio::test]
    async fn repository_scoped_replay_never_includes_organization_wide_events() {
        let store = MemoryStore::new();
        let organization_id = Uuid::now_v7();
        let repository_id = Uuid::now_v7();
        let now = Utc::now();
        store
            .append_stream_event(StreamEvent {
                id: 0,
                organization_id,
                repository_id: None,
                stream: "sync".to_string(),
                payload: serde_json::json!({ "scope": "organization" }),
                created_at: now,
            })
            .await
            .unwrap();
        store
            .append_stream_event(StreamEvent {
                id: 0,
                organization_id,
                repository_id: Some(repository_id),
                stream: "sync".to_string(),
                payload: serde_json::json!({ "scope": "repository" }),
                created_at: now,
            })
            .await
            .unwrap();

        let scoped = store
            .query_stream_events(organization_id, Some(repository_id), "sync", 0, 10)
            .await
            .unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].repository_id, Some(repository_id));

        let organization_wide = store
            .query_stream_events(organization_id, None, "sync", 0, 10)
            .await
            .unwrap();
        assert_eq!(organization_wide.len(), 2);
    }
}
