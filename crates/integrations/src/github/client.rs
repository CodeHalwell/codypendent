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
/// Per-request wall-clock ceiling so a stalled peer cannot hang a call forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Ceiling on a JSON response body (API replies are small; a huge one is wrong).
const MAX_JSON_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Ceiling on a downloaded log body.
const MAX_LOG_BODY_BYTES: usize = 64 * 1024 * 1024;
/// Most `Link: rel="next"` pages followed when scanning for an idempotency
/// marker (100 items per page). Bounds our own work on pathological repos; the
/// window (1000 items) is deep enough that a marker inside it is authoritative.
const MAX_LIST_PAGES: usize = 10;

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
        let http = Client::builder()
            .default_headers(headers)
            // A stalled peer must fail the call, not hang it indefinitely.
            .timeout(REQUEST_TIMEOUT)
            // Redirects are needed (job logs 302 to blob storage) but bounded;
            // reqwest strips `Authorization` on cross-origin redirects.
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()?;
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Ok(Self {
            http,
            base_url,
            token,
        })
    }

    /// Start a request with the fixed personal-mode headers set. The
    /// `Authorization` value is marked sensitive so any future debug rendering
    /// of the request (reqwest redacts sensitive headers) cannot print the
    /// bearer token.
    fn request(&self, method: Method, url: &str) -> reqwest::RequestBuilder {
        let builder = self.http.request(method, url);
        let raw = format!("Bearer {}", self.token.expose());
        let builder = match HeaderValue::from_str(&raw) {
            Ok(mut value) => {
                value.set_sensitive(true);
                builder.header(AUTHORIZATION, value)
            }
            // An invalid header value (impossible for a real token) keeps the
            // old behavior: the builder records the error and the send fails.
            Err(_) => builder.header(AUTHORIZATION, raw),
        };
        builder
            .header(ACCEPT, ACCEPT_VALUE)
            .header(API_VERSION_HEADER, API_VERSION_VALUE)
    }

    /// Send a request, retrying transient (`429`/`5xx`) failures with a tiny
    /// increasing backoff, and mapping non-2xx to [`GitHubError::Api`].
    ///
    /// Only non-POST requests are retried: a create POST can commit server-side
    /// and then fail on the wire (5xx / dropped response), and an automatic
    /// re-POST would duplicate the object — the list-before-create marker scan
    /// ran *before* this call, so it cannot protect an in-call retry. GET is
    /// safe; our PATCH sets absolute fields, so re-applying it is idempotent.
    async fn send(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, GitHubError> {
        let is_post = builder
            .try_clone()
            .and_then(|b| b.build().ok())
            .map(|req| req.method() == Method::POST)
            .unwrap_or(false);
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
                    let retryable = !is_post
                        && attempt < MAX_ATTEMPTS
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
            let message = read_error_snippet(response).await;
            Err(GitHubError::Api { status, message })
        }
    }

    /// GET `path` (relative to `base_url`) and deserialize the JSON body.
    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, GitHubError> {
        let url = format!("{}{}", self.base_url, path);
        let response = self.send(self.request(Method::GET, &url)).await?;
        let bytes = read_bounded(response, MAX_JSON_BODY_BYTES).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// GET a paginated array endpoint, following `Link: rel="next"` up to
    /// [`MAX_LIST_PAGES`] pages at 100 items each. Only same-origin next links
    /// are followed (a hostile `Link` header must not steer requests elsewhere).
    async fn get_json_paginated<T: DeserializeOwned>(
        &self,
        path_and_query: &str,
    ) -> Result<Vec<T>, GitHubError> {
        let separator = if path_and_query.contains('?') {
            '&'
        } else {
            '?'
        };
        let mut url = format!(
            "{}{}{}per_page=100",
            self.base_url, path_and_query, separator
        );
        let mut items: Vec<T> = Vec::new();
        for _ in 0..MAX_LIST_PAGES {
            let response = self.send(self.request(Method::GET, &url)).await?;
            let next = next_page_link(response.headers());
            let bytes = read_bounded(response, MAX_JSON_BODY_BYTES).await?;
            let page: Vec<T> = serde_json::from_slice(&bytes)?;
            items.extend(page);
            match next {
                Some(next_url) if same_origin(&next_url, &self.base_url) => url = next_url,
                _ => break,
            }
        }
        Ok(items)
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
        let bytes = read_bounded(response, MAX_JSON_BODY_BYTES).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

/// Ceiling on an error-body diagnostic snippet. Error bodies are
/// server-controlled text that flows into diagnostics (and, via tool
/// observations, into the model transcript) — an unbounded `text()` here would
/// be both an OOM surface and an oversized injection channel.
const MAX_ERROR_SNIPPET_BYTES: usize = 64 * 1024;

/// Read at most [`MAX_ERROR_SNIPPET_BYTES`] of a non-2xx body for diagnostics,
/// truncating (with a marker) rather than erroring — the status code is the
/// signal; the body is best-effort context.
async fn read_error_snippet(mut response: reqwest::Response) -> String {
    let mut bytes: Vec<u8> = Vec::new();
    let mut truncated = false;
    while let Ok(Some(chunk)) = response.chunk().await {
        let room = MAX_ERROR_SNIPPET_BYTES - bytes.len();
        if chunk.len() > room {
            bytes.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    let mut message = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        message.push_str("… [truncated]");
    }
    message
}

/// Read a response body up to `cap` bytes, erroring (never truncating silently)
/// past it — a response that big is wrong, and an unbounded read is an OOM.
async fn read_bounded(mut response: reqwest::Response, cap: usize) -> Result<Vec<u8>, GitHubError> {
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > cap {
            return Err(GitHubError::Api {
                status: 0,
                message: format!("response body exceeds the {cap}-byte ceiling"),
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Whether `next_url` stays on `base_url`'s origin. A plain prefix test is not
/// enough: `https://api.github.com.evil.com/…` starts with
/// `https://api.github.com` but is a different host, and the follow-up request
/// would carry the bearer token there. The base never ends in `/`
/// (constructor-trimmed), so a same-origin URL continues with `/`, `?`, or
/// nothing — anything else (a host character like `.` or `:`) is a different
/// origin.
fn same_origin(next_url: &str, base_url: &str) -> bool {
    match next_url.strip_prefix(base_url) {
        Some(rest) => rest.is_empty() || rest.starts_with('/') || rest.starts_with('?'),
        None => false,
    }
}

/// The `rel="next"` target of a `Link` header, if present.
fn next_page_link(headers: &HeaderMap) -> Option<String> {
    let link = headers.get("link")?.to_str().ok()?;
    for part in link.split(',') {
        let part = part.trim();
        if part.ends_with("rel=\"next\"") {
            let url = part.split(';').next()?.trim();
            return Some(
                url.trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string(),
            );
        }
    }
    None
}

/// Validate an `owner`/`repo` slug component before it is interpolated into a
/// URL path. GitHub names are ASCII alphanumerics, `-`, `_`, `.`; anything else
/// (in particular `/`, `..`, `?`, `#`, `%`) could redirect the request to a
/// different endpoint and is refused.
fn validate_slug_component(kind: &'static str, value: &str) -> Result<(), GitHubError> {
    let valid = !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if valid {
        Ok(())
    } else {
        Err(GitHubError::InvalidParameter {
            kind,
            value: value.to_string(),
        })
    }
}

/// Validate a git ref (branch, tag, or SHA) before URL interpolation. Slashes
/// are legal in branch names, but no path segment may be `.`/`..` (dot segments
/// climb the URL path) and URL metacharacters are refused. This is stricter
/// than git's own ref grammar — by design: the parameter reaches an
/// authenticated URL, so exotic names are rejected rather than encoded.
fn validate_git_ref(value: &str) -> Result<(), GitHubError> {
    let valid = !value.is_empty()
        && !value.starts_with('/')
        && !value.ends_with('/')
        && value.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        });
    if valid {
        Ok(())
    } else {
        Err(GitHubError::InvalidParameter {
            kind: "ref",
            value: value.to_string(),
        })
    }
}

/// Validate a repo id's two components in one call.
fn validate_repo(repo: &RepoId) -> Result<(), GitHubError> {
    validate_slug_component("owner", &repo.owner)?;
    validate_slug_component("repo", &repo.repo)
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
        validate_repo(repo)?;
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
        validate_repo(repo)?;
        validate_git_ref(git_ref)?;
        // Paginated like the PR/comment lists — and for the same reason: the
        // check-run idempotency scan matches `external_id` against this list,
        // and GitHub's default page is 30. A busy commit with more check runs
        // than one page would hide the marker and let a retry duplicate the
        // create. The endpoint wraps its array (`{total_count, check_runs}`),
        // so it cannot ride `get_json_paginated`'s bare-array decode.
        let mut url = format!(
            "{}/repos/{}/{}/commits/{git_ref}/check-runs?per_page=100",
            self.base_url, repo.owner, repo.repo
        );
        let mut runs: Vec<model::CheckRun> = Vec::new();
        for _ in 0..MAX_LIST_PAGES {
            let response = self.send(self.request(Method::GET, &url)).await?;
            let next = next_page_link(response.headers());
            let bytes = read_bounded(response, MAX_JSON_BODY_BYTES).await?;
            let page: CheckRunsResponse = serde_json::from_slice(&bytes)?;
            runs.extend(page.check_runs);
            match next {
                Some(next_url) if same_origin(&next_url, &self.base_url) => url = next_url,
                _ => break,
            }
        }
        Ok(runs)
    }

    async fn download_job_logs(&self, repo: &RepoId, job_id: u64) -> Result<Vec<u8>, GitHubError> {
        validate_repo(repo)?;
        let url = format!(
            "{}/repos/{}/{}/actions/jobs/{job_id}/logs",
            self.base_url, repo.owner, repo.repo
        );
        let response = self.send(self.request(Method::GET, &url)).await?;
        read_bounded(response, MAX_LOG_BODY_BYTES).await
    }

    async fn list_review_comments(
        &self,
        repo: &RepoId,
        number: u64,
    ) -> Result<Vec<model::ReviewComment>, GitHubError> {
        validate_repo(repo)?;
        self.get_json_paginated(&format!(
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
        validate_repo(repo)?;
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
        validate_repo(repo)?;
        // Idempotency: return a prior PR carrying this key, if one exists. The
        // scan pages through ALL states (a closed marked PR still proves the
        // create happened) — first-page-only, open-only scans silently missed
        // the marker on busy repos and duplicated the PR.
        let existing: Vec<model::PullRequest> = self
            .get_json_paginated(&format!(
                "/repos/{}/{}/pulls?state=all",
                repo.owner, repo.repo
            ))
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
        validate_repo(repo)?;
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
        idempotency_key: &str,
    ) -> Result<model::CheckRun, GitHubError> {
        validate_repo(repo)?;
        // Idempotency: check runs have no free-text body to mark, so the key
        // rides in `external_id`, which GitHub stores verbatim and serves back.
        // A prior run on this commit carrying the key proves the create
        // happened — return it rather than duplicating.
        for run in self.list_check_runs(repo, &req.head_sha).await? {
            if run.external_id.as_deref() == Some(idempotency_key) {
                return Ok(run);
            }
        }
        let mut payload = req.clone();
        payload.external_id = Some(idempotency_key.to_string());
        self.send_json(
            Method::POST,
            &format!("/repos/{}/{}/check-runs", repo.owner, repo.repo),
            &payload,
        )
        .await
    }
}
