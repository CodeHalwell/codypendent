//! The `reqwest`-backed [`GitHubApi`] implementation (Phase 3 STEP 3.1).
//!
//! Every request carries the fixed personal-mode headers (`Authorization`,
//! `Accept`, `X-GitHub-Api-Version`) and a static `User-Agent`. Non-2xx
//! responses map to [`GitHubError::Api`] with the body text for diagnostics;
//! `429`/`5xx` responses are retried a few times with a tiny increasing
//! backoff. Creates are idempotent: the client lists first and, finding the
//! hidden marker for a key, returns the existing object rather than duplicating
//! it. The token is never logged.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::{Client, Method};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use super::secret::GitHubToken;
use super::{idempotency, model, GitHubApi, GitHubError, RepoId};

const USER_AGENT_VALUE: &str = "codypendent/0.1";
const ACCEPT_VALUE: &str = "application/vnd.github+json";
const API_VERSION_HEADER: &str = "X-GitHub-Api-Version";
const API_VERSION_VALUE: &str = "2022-11-28";

const MAX_ATTEMPTS: u32 = 3;

/// The personal-mode REST GitHub client.
pub struct RestGitHubClient {
    http: Client,
    base_url: String,
    token: GitHubToken,
}

/// The wrapper GitHub returns from the check-runs endpoint.
#[derive(Deserialize)]
struct CheckRunsResponse {
    #[serde(default)]
    check_runs: Vec<model::CheckRun>,
}

impl RestGitHubClient {
    /// Build a client against `base_url` (e.g. `https://api.github.com` or a
    /// mock server URL). A trailing slash is stripped so path joins are exact.
    pub fn new(base_url: impl Into<String>, token: GitHubToken) -> Result<Self, GitHubError> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        let http = Client::builder().default_headers(headers).build()?;
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Ok(Self {
            http,
            base_url,
            token,
        })
    }

    /// Start a request with the fixed personal-mode headers set.
    fn request(&self, method: Method, url: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token.expose()))
            .header(ACCEPT, ACCEPT_VALUE)
            .header(API_VERSION_HEADER, API_VERSION_VALUE)
    }

    /// Send a request, retrying transient (`429`/`5xx`) failures with a tiny
    /// increasing backoff, and mapping non-2xx to [`GitHubError::Api`].
    async fn send(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, GitHubError> {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let this_try = match builder.try_clone() {
                Some(cloned) => cloned,
                // Non-cloneable (streaming) body — send once, no retry.
                None => return self.execute(builder).await,
            };
            match self.execute(this_try).await {
                Ok(response) => return Ok(response),
                Err(err) => {
                    let retryable = attempt < MAX_ATTEMPTS
                        && matches!(&err, GitHubError::Api { status, .. } if is_retryable(*status));
                    if retryable {
                        // 50ms, then 100ms — small enough to keep tests fast.
                        let delay = 50u64 * 2u64.pow(attempt - 1);
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }

    /// Execute one request, mapping a non-2xx status to [`GitHubError::Api`].
    async fn execute(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, GitHubError> {
        let response = builder.send().await?;
        if response.status().is_success() {
            Ok(response)
        } else {
            let status = response.status().as_u16();
            let message = response.text().await.unwrap_or_default();
            Err(GitHubError::Api { status, message })
        }
    }

    /// GET `path` (relative to `base_url`) and deserialize the JSON body.
    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, GitHubError> {
        let url = format!("{}{}", self.base_url, path);
        let response = self.send(self.request(Method::GET, &url)).await?;
        let bytes = response.bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Send `body` as JSON to `path` with `method` and deserialize the reply.
    async fn send_json<B: serde::Serialize, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &B,
    ) -> Result<T, GitHubError> {
        let url = format!("{}{}", self.base_url, path);
        let response = self.send(self.request(method, &url).json(body)).await?;
        let bytes = response.bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

/// Whether a status warrants a retry: rate limiting or a server-side failure.
fn is_retryable(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

#[async_trait]
impl GitHubApi for RestGitHubClient {
    async fn get_pull_request(
        &self,
        repo: &RepoId,
        number: u64,
    ) -> Result<model::PullRequest, GitHubError> {
        self.get_json(&format!(
            "/repos/{}/{}/pulls/{number}",
            repo.owner, repo.repo
        ))
        .await
    }

    async fn list_check_runs(
        &self,
        repo: &RepoId,
        git_ref: &str,
    ) -> Result<Vec<model::CheckRun>, GitHubError> {
        let wrapper: CheckRunsResponse = self
            .get_json(&format!(
                "/repos/{}/{}/commits/{git_ref}/check-runs",
                repo.owner, repo.repo
            ))
            .await?;
        Ok(wrapper.check_runs)
    }

    async fn download_job_logs(&self, repo: &RepoId, job_id: u64) -> Result<Vec<u8>, GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/actions/jobs/{job_id}/logs",
            self.base_url, repo.owner, repo.repo
        );
        let response = self.send(self.request(Method::GET, &url)).await?;
        let bytes = response.bytes().await?;
        Ok(bytes.to_vec())
    }

    async fn list_review_comments(
        &self,
        repo: &RepoId,
        number: u64,
    ) -> Result<Vec<model::ReviewComment>, GitHubError> {
        self.get_json(&format!(
            "/repos/{}/{}/issues/{number}/comments",
            repo.owner, repo.repo
        ))
        .await
    }

    async fn create_review_comment(
        &self,
        repo: &RepoId,
        number: u64,
        body: &str,
        idempotency_key: &str,
    ) -> Result<model::ReviewComment, GitHubError> {
        // Idempotency: return a prior comment carrying this key, if one exists.
        for comment in self.list_review_comments(repo, number).await? {
            if idempotency::body_matches_key(&comment.body, idempotency_key) {
                return Ok(comment);
            }
        }
        let marked = idempotency::body_with_marker(body, idempotency_key);
        let payload = serde_json::json!({ "body": marked });
        self.send_json(
            Method::POST,
            &format!(
                "/repos/{}/{}/issues/{number}/comments",
                repo.owner, repo.repo
            ),
            &payload,
        )
        .await
    }

    async fn create_draft_pull_request(
        &self,
        repo: &RepoId,
        req: &model::NewPullRequest,
        idempotency_key: &str,
    ) -> Result<model::PullRequest, GitHubError> {
        // Idempotency: return a prior open PR carrying this key, if one exists.
        let existing: Vec<model::PullRequest> = self
            .get_json(&format!("/repos/{}/{}/pulls", repo.owner, repo.repo))
            .await?;
        for pr in existing {
            if let Some(body) = &pr.body {
                if idempotency::body_matches_key(body, idempotency_key) {
                    return Ok(pr);
                }
            }
        }
        let marked =
            idempotency::body_with_marker(req.body.as_deref().unwrap_or_default(), idempotency_key);
        let mut payload = req.clone();
        payload.body = Some(marked);
        self.send_json(
            Method::POST,
            &format!("/repos/{}/{}/pulls", repo.owner, repo.repo),
            &payload,
        )
        .await
    }

    async fn update_pull_request(
        &self,
        repo: &RepoId,
        number: u64,
        req: &model::UpdatePullRequest,
    ) -> Result<model::PullRequest, GitHubError> {
        self.send_json(
            Method::PATCH,
            &format!("/repos/{}/{}/pulls/{number}", repo.owner, repo.repo),
            req,
        )
        .await
    }

    async fn create_check_run_summary(
        &self,
        repo: &RepoId,
        req: &model::NewCheckRun,
    ) -> Result<model::CheckRun, GitHubError> {
        self.send_json(
            Method::POST,
            &format!("/repos/{}/{}/check-runs", repo.owner, repo.repo),
            req,
        )
        .await
    }
}
