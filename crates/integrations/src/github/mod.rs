//! GitHub personal-mode client (Phase 3 STEP 3.1).
//!
//! A typed, minimal GitHub REST surface for personal ("bring your own token")
//! mode. The design keeps three concerns separate:
//!
//! - [`GitHubApi`] — the transport-agnostic trait the tool layer calls. The
//!   daemon depends on the trait, never the concrete client, so tests can
//!   substitute a fake and the policy engine can wrap every write.
//! - [`secret`] — the token broker: the token is read from `gh auth token` or
//!   `GITHUB_TOKEN`, held in an opaque [`GitHubToken`] that never leaks its
//!   value into `Debug`, logs, or any serializable type.
//! - [`idempotency`] — a hidden HTML-comment marker embedded in created bodies
//!   so a retried command finds and returns its prior object instead of
//!   creating a duplicate.
//!
//! Every mutating call is surfaced to the policy engine as a
//! [`github_mutation_action`] before it runs (Chapter 14): a
//! [`codypendent_protocol::ProposedAction::GitHubMutation`] carrying the target
//! slug and a short human-readable summary for the approval card.

pub mod client;
pub mod idempotency;
pub mod model;
pub mod secret;

pub use client::RestGitHubClient;
pub use secret::GitHubToken;

use async_trait::async_trait;
use codypendent_protocol::ProposedAction;

/// A GitHub repository identity — an `owner` and a `repo` name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoId {
    /// The repository owner (user or organization) login.
    pub owner: String,
    /// The repository name.
    pub repo: String,
}

impl RepoId {
    /// Construct a [`RepoId`] from an owner and repo name.
    pub fn new(owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            repo: repo.into(),
        }
    }

    /// The canonical `owner/repo` slug, as GitHub renders it and as the policy
    /// engine keys repository-scoped approvals.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// Errors from the GitHub client: HTTP transport, non-2xx API responses,
/// missing credentials, (de)serialization, and local I/O (e.g. spawning `gh`).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GitHubError {
    /// The underlying HTTP transport failed (connect, TLS, body read).
    #[error("github http transport error: {0}")]
    Http(#[from] reqwest::Error),

    /// GitHub returned a non-2xx status. The `message` is the response body
    /// text, which never contains the token.
    #[error("github api error (status {status}): {message}")]
    Api {
        /// The HTTP status code.
        status: u16,
        /// The response body, used for diagnostics.
        message: String,
    },

    /// No usable token could be found. The payload names the source that was
    /// tried (e.g. `GITHUB_TOKEN` or `gh auth token`) — never a token value.
    #[error("missing github token: {0}")]
    MissingToken(String),

    /// A request or response payload could not be (de)serialized.
    #[error("github payload (de)serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// A local I/O operation failed (e.g. spawning the `gh` CLI).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// The typed GitHub surface the tool layer depends on. Reads and writes are
/// both here; the daemon wraps every mutating call in policy evaluation via
/// [`github_mutation_action`] first.
#[async_trait]
pub trait GitHubApi: Send + Sync {
    /// Fetch a single pull request by number.
    async fn get_pull_request(
        &self,
        repo: &RepoId,
        number: u64,
    ) -> Result<model::PullRequest, GitHubError>;

    /// List the check runs for a git ref (commit SHA, branch, or tag).
    async fn list_check_runs(
        &self,
        repo: &RepoId,
        git_ref: &str,
    ) -> Result<Vec<model::CheckRun>, GitHubError>;

    /// Download the raw logs for a single Actions job.
    async fn download_job_logs(&self, repo: &RepoId, job_id: u64) -> Result<Vec<u8>, GitHubError>;

    /// List the issue/review comments on a pull request.
    async fn list_review_comments(
        &self,
        repo: &RepoId,
        number: u64,
    ) -> Result<Vec<model::ReviewComment>, GitHubError>;

    /// Create a review comment, idempotently: if a comment already carries the
    /// hidden marker for `idempotency_key`, that existing comment is returned
    /// and no new comment is created.
    async fn create_review_comment(
        &self,
        repo: &RepoId,
        number: u64,
        body: &str,
        idempotency_key: &str,
    ) -> Result<model::ReviewComment, GitHubError>;

    /// Create a draft pull request, idempotently: if an open PR already carries
    /// the hidden marker for `idempotency_key`, that existing PR is returned.
    async fn create_draft_pull_request(
        &self,
        repo: &RepoId,
        req: &model::NewPullRequest,
        idempotency_key: &str,
    ) -> Result<model::PullRequest, GitHubError>;

    /// Update fields (title, body, state) of an existing pull request.
    async fn update_pull_request(
        &self,
        repo: &RepoId,
        number: u64,
        req: &model::UpdatePullRequest,
    ) -> Result<model::PullRequest, GitHubError>;

    /// Create a check-run summary against a commit SHA.
    async fn create_check_run_summary(
        &self,
        repo: &RepoId,
        req: &model::NewCheckRun,
    ) -> Result<model::CheckRun, GitHubError>;
}

/// Build the policy-visible action for a GitHub write. The daemon feeds this to
/// the policy engine before every mutating call so the write is approval-gated
/// and network-scoped to the GitHub API (Chapter 14).
pub fn github_mutation_action(repo: &RepoId, summary: impl Into<String>) -> ProposedAction {
    ProposedAction::GitHubMutation {
        repository: repo.slug(),
        summary: summary.into(),
    }
}
