//! Typed models for the subset of the GitHub REST API this client uses.
//!
//! Response types derive [`Deserialize`] and lean on `#[serde(default)]` so a
//! partial payload (or a newer, richer one) degrades gracefully. Request bodies
//! derive [`Serialize`] and skip absent optional fields, matching GitHub's
//! "omit to leave unchanged" semantics.

use serde::{Deserialize, Serialize};

/// A git ref reference as embedded in a pull request's `head`/`base` objects.
/// Only the ref name is modeled; the wire key is `ref`, a Rust keyword, so the
/// field is renamed.
#[derive(Debug, Clone, Deserialize)]
pub struct GitRef {
    /// The branch/ref name (JSON key `ref`).
    #[serde(rename = "ref")]
    pub ref_name: String,
}

/// A pull request, as returned by `GET /repos/{owner}/{repo}/pulls/{number}`
/// and the list endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequest {
    /// The PR number within the repository.
    pub number: u64,
    /// The PR title.
    #[serde(default)]
    pub title: String,
    /// The PR body (may be absent/null).
    #[serde(default)]
    pub body: Option<String>,
    /// The PR state (`open`, `closed`).
    #[serde(default)]
    pub state: String,
    /// Whether the PR is a draft.
    #[serde(default)]
    pub draft: bool,
    /// The web URL of the PR.
    #[serde(default)]
    pub html_url: String,
    /// The head ref (source branch).
    #[serde(default)]
    pub head: Option<GitRef>,
    /// The base ref (target branch).
    #[serde(default)]
    pub base: Option<GitRef>,
}

impl PullRequest {
    /// The head branch ref name, if present.
    pub fn head_ref(&self) -> Option<&str> {
        self.head.as_ref().map(|r| r.ref_name.as_str())
    }

    /// The base branch ref name, if present.
    pub fn base_ref(&self) -> Option<&str> {
        self.base.as_ref().map(|r| r.ref_name.as_str())
    }
}

/// A single check run, as returned by the check-runs endpoint and check-run
/// creation.
#[derive(Debug, Clone, Deserialize)]
pub struct CheckRun {
    /// The check-run id.
    pub id: u64,
    /// The check-run name.
    #[serde(default)]
    pub name: String,
    /// The status (`queued`, `in_progress`, `completed`).
    #[serde(default)]
    pub status: String,
    /// The conclusion (`success`, `failure`, ...), present once completed.
    #[serde(default)]
    pub conclusion: Option<String>,
    /// The creator-supplied replay-safety marker, if one was set (see
    /// [`NewCheckRun::external_id`]). GitHub serves `""` for runs created
    /// without one.
    #[serde(default)]
    pub external_id: Option<String>,
}

/// The author of a comment.
#[derive(Debug, Clone, Deserialize)]
pub struct User {
    /// The user's login.
    #[serde(default)]
    pub login: String,
}

/// An issue/review comment on a pull request.
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewComment {
    /// The comment id.
    pub id: u64,
    /// The comment body (carries the hidden idempotency marker when created by
    /// this client).
    #[serde(default)]
    pub body: String,
    /// The comment author, if present.
    #[serde(default)]
    pub user: Option<User>,
}

impl ReviewComment {
    /// The author's login, if present.
    pub fn user_login(&self) -> Option<&str> {
        self.user.as_ref().map(|u| u.login.as_str())
    }
}

/// The request body for creating a pull request.
#[derive(Debug, Clone, Serialize)]
pub struct NewPullRequest {
    /// The PR title.
    pub title: String,
    /// The PR body (the idempotency marker is appended before posting).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// The head ref (source branch).
    pub head: String,
    /// The base ref (target branch).
    pub base: String,
    /// Whether to open the PR as a draft.
    pub draft: bool,
}

impl NewPullRequest {
    /// A draft PR request with no body. Draft mode is the personal-mode default
    /// (writes are surfaced for approval; nothing merges unattended).
    pub fn draft(
        title: impl Into<String>,
        head: impl Into<String>,
        base: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            body: None,
            head: head.into(),
            base: base.into(),
            draft: true,
        }
    }
}

/// The request body for updating a pull request. Absent fields are left
/// unchanged by GitHub.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdatePullRequest {
    /// A new title, if changing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A new body, if changing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// A new state (`open`/`closed`), if changing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// The request body for creating a check-run summary against a commit.
///
/// The caller-facing shape is flat, but the wire shape is not: GitHub's
/// create-check-run endpoint takes the human-readable text nested under
/// `output` (with a mandatory `title`) and *silently ignores* unknown
/// top-level fields — a flat `summary` would be dropped without error. The
/// manual [`Serialize`] impl below emits the nested wire shape.
#[derive(Debug, Clone)]
pub struct NewCheckRun {
    /// The check-run name (also used as the `output.title`).
    pub name: String,
    /// The commit SHA the check run reports on.
    pub head_sha: String,
    /// A short human-readable summary (sent as `output.summary`).
    pub summary: String,
    /// The conclusion (`success`, `failure`, ...), if the run is complete.
    /// GitHub infers `status: completed` when a conclusion is present.
    pub conclusion: Option<String>,
    /// The replay-safety marker: stored by GitHub verbatim and read back when
    /// scanning for a prior create with the same key (the check-run analogue
    /// of the hidden body marker — check runs have no free-text body to mark).
    pub external_id: Option<String>,
}

impl Serialize for NewCheckRun {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        #[derive(Serialize)]
        struct Output<'a> {
            title: &'a str,
            summary: &'a str,
        }
        let mut fields = 3;
        fields += usize::from(self.conclusion.is_some());
        fields += usize::from(self.external_id.is_some());
        let mut state = serializer.serialize_struct("NewCheckRun", fields)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("head_sha", &self.head_sha)?;
        if let Some(conclusion) = &self.conclusion {
            state.serialize_field("conclusion", conclusion)?;
        }
        if let Some(external_id) = &self.external_id {
            state.serialize_field("external_id", external_id)?;
        }
        state.serialize_field(
            "output",
            &Output {
                title: &self.name,
                summary: &self.summary,
            },
        )?;
        state.end()
    }
}
