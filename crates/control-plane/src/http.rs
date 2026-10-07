use axum::{
    http::HeaderValue,
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::{
    health::{healthz, readyz, version},
    routes::{
        audit::list_audit_records,
        auth::{complete_pairing, link_identity, login, refresh, start_pairing_challenge},
        objects::{download_object, get_object_metadata, presign_object_url, upload_object},
        orgs::{add_member, create_organization, get_organization, list_organizations},
        repos::{get_repository, list_repositories, register_repository},
        sync::{list_shared_sessions, pull_sync_events, push_sync_envelope},
    },
    state::AppState,
    ws::{issue_ws_ticket, ws_handler},
};

pub fn build_router(state: AppState) -> Router {
    let configured_origins = &state.config.cors_allowed_origins;
    let cors = if configured_origins.iter().any(|origin| origin == "*") {
        CorsLayer::new().allow_origin(Any)
    } else {
        // Invalid origins are denied rather than widening to `Any`. Config
        // validation can report them at startup in a future typed config pass;
        // the request boundary remains fail-closed today.
        let origins = configured_origins
            .iter()
            .filter_map(|origin| HeaderValue::from_str(origin).ok())
            .collect::<Vec<_>>();
        CorsLayer::new().allow_origin(AllowOrigin::list(origins))
    }
    .allow_methods(Any)
    .allow_headers(Any);

    Router::new()
        // Health & version routes
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/version", get(version))
        // Authentication & Identity routes
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/refresh", post(refresh))
        .route("/v1/auth/pairing/challenge", post(start_pairing_challenge))
        .route("/v1/auth/pairing/complete", post(complete_pairing))
        .route("/v1/auth/link", post(link_identity))
        // Organization management
        .route(
            "/v1/organizations",
            post(create_organization).get(list_organizations),
        )
        .route("/v1/organizations/{id}", get(get_organization))
        .route("/v1/organizations/{id}/members", post(add_member))
        // Repository management
        .route(
            "/v1/organizations/{org_id}/repositories",
            post(register_repository).get(list_repositories),
        )
        .route(
            "/v1/organizations/{org_id}/repositories/{id}",
            get(get_repository),
        )
        // Sync & Sessions
        // Accepts the protocol's batched `SyncEnvelope`.
        .route("/v1/sync/push", post(push_sync_envelope))
        .route("/v1/sync/pull", get(pull_sync_events))
        .route(
            "/v1/organizations/{org_id}/sessions",
            get(list_shared_sessions),
        )
        // Published Objects
        .route(
            "/v1/organizations/{org_id}/objects/upload",
            post(upload_object),
        )
        .route(
            "/v1/organizations/{org_id}/objects/presign",
            post(presign_object_url),
        )
        .route(
            "/v1/organizations/{org_id}/objects/{hash}",
            get(download_object),
        )
        .route(
            "/v1/organizations/{org_id}/objects/{hash}/metadata",
            get(get_object_metadata),
        )
        // Audit Logs
        .route("/v1/organizations/{org_id}/audit", get(list_audit_records))
        // WebSocket Realtime Events Stream
        .route("/v1/events/ticket", post(issue_ws_ticket))
        .route("/v1/events/stream", get(ws_handler))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn serve(addr: SocketAddr, state: AppState) -> Result<(), std::io::Error> {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("Control plane listening on {}", addr);
    axum::serve(listener, app).await
}
