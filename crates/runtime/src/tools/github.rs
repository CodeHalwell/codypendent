//! GitHub tools (Phase 3 STEP 3.2).
//!
//! Thin argument-parsing and action-building glue between the agent loop and the
//! `codypendent-integrations` GitHub client. The loop holds the client handle (a
//! shared `Arc<dyn GitHubApi>`) and the per-run [`RepoId`]; these tools only
//! parse the model's arguments and name the policy-visible [`ProposedAction`] —
//! the loop's middleware runs that action through the policy engine and, for a
//! mutation, parks it for approval before [`crate::agent`] calls the client.
//!
//! Reads (`github.get_pull_request`, `github.list_check_runs`) are network reads
//! to the GitHub API: [`ProposedAction::NetworkRequest`], allowed when the
//! endpoint is on the network policy's allow-list and the mode permits network,
//! never requiring approval. The write (`github.create_draft_pull_request`) is a
//! [`ProposedAction::GitHubMutation`], which the policy engine always sends
//! through approval.

// Single source of truth for the endpoint: the policy engine owns it (a GitHub
// mutation must be network-authorized against exactly this string), and the tool
// layer reuses it so a read's `NetworkRequest` destination can never drift out of
// sync with what the policy admits.
use codypendent_daemon::policy::GITHUB_API_ENDPOINT;
use codypendent_integrations::github::model::{CheckRun, NewPullRequest, PullRequest};
use codypendent_integrations::github::{github_mutation_action, RepoId};
use codypendent_protocol::ProposedAction;
use serde_json::Value;

/// The typed input for `github.get_pull_request`.
pub struct GetPullRequestInput {
    /// The PR number to fetch.
    pub number: u64,
}

/// Read a pull request by number.
pub struct GetPullRequest;

impl GetPullRequest {
    /// The stable dotted tool name.
    pub const NAME: &'static str = "github.get_pull_request";

    /// A GitHub read is a network request to the API endpoint (no approval).
    pub fn proposed_action() -> ProposedAction {
        ProposedAction::NetworkRequest {
            destination: GITHUB_API_ENDPOINT.to_string(),
        }
    }
}

/// Parse `github.get_pull_request` arguments.
pub fn parse_get_pull_request(args: &Value) -> Result<GetPullRequestInput, String> {
    let number = args
        .get("number")
        .and_then(Value::as_u64)
        .ok_or("github.get_pull_request requires an integer `number`")?;
    Ok(GetPullRequestInput { number })
}

/// The typed input for `github.list_check_runs`.
pub struct ListCheckRunsInput {
    /// The git ref (commit SHA, branch, or tag) whose checks to list.
    pub git_ref: String,
}

/// List the check runs for a git ref.
pub struct ListCheckRuns;

impl ListCheckRuns {
    /// The stable dotted tool name.
    pub const NAME: &'static str = "github.list_check_runs";

    /// A GitHub read is a network request to the API endpoint (no approval).
    pub fn proposed_action() -> ProposedAction {
        ProposedAction::NetworkRequest {
            destination: GITHUB_API_ENDPOINT.to_string(),
        }
    }
}

/// Parse `github.list_check_runs` arguments (`ref` or `git_ref`).
pub fn parse_list_check_runs(args: &Value) -> Result<ListCheckRunsInput, String> {
    let git_ref = args
        .get("ref")
        .or_else(|| args.get("git_ref"))
        .and_then(Value::as_str)
        .ok_or("github.list_check_runs requires a string `ref`")?;
    Ok(ListCheckRunsInput {
        git_ref: git_ref.to_string(),
    })
}

/// The typed input for `github.create_draft_pull_request`.
pub struct CreateDraftPullRequestInput {
    /// The PR title.
    pub title: String,
    /// The head (source) branch.
    pub head: String,
    /// The base (target) branch.
    pub base: String,
    /// The PR body, if any.
    pub body: Option<String>,
    /// The idempotency key embedded as a hidden marker so a retried create finds
    /// the existing PR. Derived from the head→base pair (a repo's PR identity).
    pub idempotency_key: String,
}

/// Create a draft pull request.
pub struct CreateDraftPullRequest;

impl CreateDraftPullRequest {
    /// The stable dotted tool name.
    pub const NAME: &'static str = "github.create_draft_pull_request";

    /// A GitHub write is a mutation, always approval-gated by the policy engine.
    pub fn proposed_action(repo: &RepoId) -> ProposedAction {
        github_mutation_action(repo, format!("create draft PR on {}", repo.slug()))
    }
}

/// Parse `github.create_draft_pull_request` arguments.
pub fn parse_create_draft_pull_request(
    args: &Value,
) -> Result<CreateDraftPullRequestInput, String> {
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .ok_or("github.create_draft_pull_request requires a string `title`")?
        .to_string();
    let head = args
        .get("head")
        .and_then(Value::as_str)
        .ok_or("github.create_draft_pull_request requires a string `head`")?
        .to_string();
    let base = args
        .get("base")
        .and_then(Value::as_str)
        .ok_or("github.create_draft_pull_request requires a string `base`")?
        .to_string();
    let body = args.get("body").and_then(Value::as_str).map(str::to_string);
    // The head→base pair identifies a PR within a repository, so a re-issued
    // create for the same branch pair resolves (via the hidden marker) to the
    // existing PR instead of a duplicate. `:` is forbidden in git ref names, so
    // it is an unambiguous delimiter — no branch-name pair can collide on it.
    let idempotency_key = format!("{head}:{base}");
    Ok(CreateDraftPullRequestInput {
        title,
        head,
        base,
        body,
        idempotency_key,
    })
}

/// Build the client request body from a parsed input.
pub fn new_pull_request(input: &CreateDraftPullRequestInput) -> NewPullRequest {
    let mut request =
        NewPullRequest::draft(input.title.clone(), input.head.clone(), input.base.clone());
    request.body = input.body.clone();
    request
}

/// Render a fetched pull request as a compact observation for the transcript.
pub fn render_pull_request(pr: &PullRequest) -> String {
    format!(
        "PR #{} [{}{}]: {}\n{}",
        pr.number,
        pr.state,
        if pr.draft { ", draft" } else { "" },
        pr.title,
        pr.html_url
    )
}

/// Render a list of check runs as a compact observation for the transcript.
pub fn render_check_runs(runs: &[CheckRun]) -> String {
    if runs.is_empty() {
        return "no check runs".to_string();
    }
    let mut lines = Vec::with_capacity(runs.len() + 1);
    lines.push(format!("{} check run(s):", runs.len()));
    for run in runs {
        lines.push(format!(
            "- {} [{}/{}]",
            run.name,
            run.status,
            run.conclusion.as_deref().unwrap_or("pending")
        ));
    }
    lines.join("\n")
}
