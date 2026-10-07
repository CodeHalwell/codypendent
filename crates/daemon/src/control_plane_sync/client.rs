//! Outbound HTTP client for control plane API interactions.
//!
//! Note: Outbound only. The daemon initiates all connections; no local listening port is ever opened.

use std::time::Duration;

use codypendent_control_plane_protocol::{
    Repository, StreamEvent, SyncBatchResponse, SyncEnvelope,
};
use reqwest::{header, Client, StatusCode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::error::ControlPlaneSyncError;

const CONTROL_PLANE_ERROR_BODY_LIMIT: usize = 512;

fn clamp_error_body(mut body: String) -> String {
    if body.len() > CONTROL_PLANE_ERROR_BODY_LIMIT {
        let mut boundary = CONTROL_PLANE_ERROR_BODY_LIMIT;
        while !body.is_char_boundary(boundary) {
            boundary -= 1;
        }
        body.truncate(boundary);
        body.push_str("… [truncated]");
    }
    body
}

async fn bounded_error_body(response: reqwest::Response) -> String {
    clamp_error_body(response.text().await.unwrap_or_default())
}

/// Request to complete a pairing challenge with the control plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletePairingRequest {
    pub pairing_code: String,
    pub display_name: String,
    pub consent_manifest: String,
    pub max_publication_class: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepts_remote_approvals: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepts_runner_dispatch: Option<bool>,
}

/// Response returned by the control plane upon successful pairing completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletePairingResponse {
    pub daemon_id: Uuid,
    pub organization_id: Uuid,
    pub token: String,
}

/// Backwards-compatible names for the shared batch wire contract. The old
/// private flat DTOs never interoperated with the Axum route.
pub type SyncDeltaPushRequest = SyncEnvelope;
pub type SyncDeltaPushResponse = SyncBatchResponse;

/// Client interacting with a remote control plane service.
#[derive(Debug, Clone)]
pub struct ControlPlaneClient {
    endpoint: String,
    http: Client,
    token: Option<String>,
}

impl ControlPlaneClient {
    /// Create a new client targeting a specific control plane endpoint.
    pub fn new(endpoint: &str, token: Option<String>) -> Result<Self, ControlPlaneSyncError> {
        let trimmed_endpoint = endpoint.trim().trim_end_matches('/').to_string();
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(10))
            .build()?;

        Ok(Self {
            endpoint: trimmed_endpoint,
            http,
            token,
        })
    }

    /// Set or update the bearer access token.
    #[must_use]
    pub fn with_token(mut self, token: String) -> Self {
        self.token = Some(token);
        self
    }

    /// Complete a pairing handshake by exchanging a pairing challenge code.
    pub async fn complete_pairing(
        &self,
        request: &CompletePairingRequest,
    ) -> Result<CompletePairingResponse, ControlPlaneSyncError> {
        let url = format!("{}/v1/auth/pairing/complete", self.endpoint);
        let resp = self.http.post(&url).json(request).send().await?;

        if resp.status().is_success() {
            let data = resp.json::<CompletePairingResponse>().await?;
            Ok(data)
        } else if resp.status() == StatusCode::UNAUTHORIZED
            || resp.status() == StatusCode::FORBIDDEN
        {
            let body = bounded_error_body(resp).await;
            Err(ControlPlaneSyncError::RemoteRejected(format!(
                "pairing challenge rejected: {body}"
            )))
        } else {
            let status = resp.status();
            let body = bounded_error_body(resp).await;
            Err(ControlPlaneSyncError::RemoteRejected(format!(
                "pairing failed with status {status}: {body}"
            )))
        }
    }

    /// Push an outbound synchronization envelope to the control plane.
    pub async fn push_sync_envelope(
        &self,
        envelope: &SyncEnvelope,
    ) -> Result<SyncBatchResponse, ControlPlaneSyncError> {
        let url = format!("{}/v1/sync/push", self.endpoint);
        let mut builder = self.http.post(&url);

        if let Some(ref tok) = self.token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {tok}"));
        }

        let resp = builder.json(envelope).send().await?;

        if resp.status().is_success() {
            let data = resp.json::<SyncBatchResponse>().await?;
            Ok(data)
        } else if resp.status() == StatusCode::UNAUTHORIZED {
            Err(ControlPlaneSyncError::Revoked(
                "control plane rejected daemon credentials (revoked or expired)".to_string(),
            ))
        } else {
            let status = resp.status();
            let body = bounded_error_body(resp).await;
            Err(ControlPlaneSyncError::RemoteRejected(format!(
                "sync push failed with status {status}: {body}"
            )))
        }
    }

    /// List the repositories visible to this pairing's organization.
    ///
    /// Consent manifests retain cross-machine repository identities, while the
    /// sync routes require control-plane UUIDs. This authenticated catalog is
    /// the authority used to bridge those two identifier domains.
    pub async fn list_repositories(
        &self,
        organization_id: Uuid,
    ) -> Result<Vec<Repository>, ControlPlaneSyncError> {
        let url = format!(
            "{}/v1/organizations/{organization_id}/repositories",
            self.endpoint
        );
        let mut builder = self.http.get(&url);

        if let Some(ref tok) = self.token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {tok}"));
        }

        let resp = builder.send().await?;
        if resp.status().is_success() {
            Ok(resp.json::<Vec<Repository>>().await?)
        } else if resp.status() == StatusCode::UNAUTHORIZED
            || resp.status() == StatusCode::FORBIDDEN
        {
            Err(ControlPlaneSyncError::Revoked(
                "control plane rejected daemon credentials (revoked or expired)".to_string(),
            ))
        } else {
            let status = resp.status();
            let body = bounded_error_body(resp).await;
            Err(ControlPlaneSyncError::RemoteRejected(format!(
                "repository catalog request failed with status {status}: {body}"
            )))
        }
    }

    /// Pull stream events from the control plane starting from a cursor.
    pub async fn pull_sync_events(
        &self,
        repository_id: Option<Uuid>,
        stream: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<StreamEvent>, ControlPlaneSyncError> {
        let url = format!("{}/v1/sync/pull", self.endpoint);
        let mut query = vec![
            ("stream", stream.to_string()),
            ("after_id", after_id.to_string()),
            ("limit", limit.to_string()),
        ];
        if let Some(repository_id) = repository_id {
            query.push(("repository_id", repository_id.to_string()));
        }
        let mut builder = self.http.get(&url).query(&query);

        if let Some(ref tok) = self.token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {tok}"));
        }

        let resp = builder.send().await?;

        if resp.status().is_success() {
            let events = resp.json::<Vec<StreamEvent>>().await?;
            Ok(events)
        } else if resp.status() == StatusCode::UNAUTHORIZED {
            Err(ControlPlaneSyncError::Revoked(
                "control plane rejected daemon credentials (revoked or expired)".to_string(),
            ))
        } else {
            let status = resp.status();
            let body = bounded_error_body(resp).await;
            Err(ControlPlaneSyncError::RemoteRejected(format!(
                "sync pull failed with status {status}: {body}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        extract::{Path, Query},
        http::{header::AUTHORIZATION, HeaderMap},
        routing::get,
        Json, Router,
    };
    use codypendent_control_plane_protocol::{
        DataClassification, FederatedRepositoryId, OrganizationId, RepositoryId,
    };
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn remote_error_bodies_are_utf8_safely_bounded() {
        let body = format!("{}é{}", "a".repeat(511), "b".repeat(100));
        assert!(!body.is_char_boundary(CONTROL_PLANE_ERROR_BODY_LIMIT));
        let bounded = clamp_error_body(body);
        assert!(bounded.ends_with("… [truncated]"));
        assert!(bounded.len() <= CONTROL_PLANE_ERROR_BODY_LIMIT + "… [truncated]".len());
    }

    #[tokio::test]
    async fn repository_catalog_uses_the_scoped_authenticated_route() {
        let organization_id = Uuid::now_v7();
        let repository_id = Uuid::now_v7();
        let expected = Repository {
            id: RepositoryId::from_uuid(repository_id),
            organization_id: OrganizationId::from_uuid(organization_id),
            federated_id: FederatedRepositoryId::new("a".repeat(64))
                .expect("valid federated identity"),
            display_name: "Repository".to_string(),
            max_publication_class:
                codypendent_control_plane_protocol::PublicationClass::MetadataShared,
            max_classification: DataClassification::Internal,
            policy_version: 1,
            created_at: chrono::Utc::now(),
        };
        let response_repository = expected.clone();
        let app = Router::new().route(
            "/v1/organizations/{organization_id}/repositories",
            get(
                move |Path(requested_organization_id): Path<Uuid>, headers: HeaderMap| {
                    let repository = response_repository.clone();
                    async move {
                        assert_eq!(requested_organization_id, organization_id);
                        assert_eq!(
                            headers
                                .get(AUTHORIZATION)
                                .and_then(|value| value.to_str().ok()),
                            Some("Bearer workload-token")
                        );
                        Json(vec![repository])
                    }
                },
            ),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("local address"));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let client =
            ControlPlaneClient::new(&endpoint, Some("workload-token".to_string())).expect("client");
        let catalog = client
            .list_repositories(organization_id)
            .await
            .expect("repository catalog");

        assert_eq!(catalog, vec![expected]);
        server.abort();
    }

    #[tokio::test]
    async fn organization_policy_pull_omits_repository_query_scope() {
        let app = Router::new().route(
            "/v1/sync/pull",
            get(
                |Query(query): Query<std::collections::HashMap<String, String>>,
                 headers: HeaderMap| async move {
                    assert_eq!(query.get("stream").map(String::as_str), Some("policy"));
                    assert!(!query.contains_key("repository_id"));
                    assert_eq!(query.get("after_id").map(String::as_str), Some("17"));
                    assert_eq!(
                        headers
                            .get(AUTHORIZATION)
                            .and_then(|value| value.to_str().ok()),
                        Some("Bearer workload-token")
                    );
                    Json(Vec::<StreamEvent>::new())
                },
            ),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("local address"));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let client =
            ControlPlaneClient::new(&endpoint, Some("workload-token".to_string())).expect("client");
        let events = client
            .pull_sync_events(None, "policy", 17, 50)
            .await
            .expect("organization policy pull");

        assert!(events.is_empty());
        server.abort();
    }
}
