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
use codypendent_integrations::github::model::{
    CheckRun, NewCheckRun, NewPullRequest, PullRequest, UpdatePullRequest,
};
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

/// The typed input for `github.update_pull_request`.
pub struct UpdatePullRequestInput {
    /// The PR number to update.
    pub number: u64,
    /// The request body (title/body/state, each optional).
    pub request: UpdatePullRequest,
}

/// Update an existing pull request (title, body, state).
pub struct UpdatePullRequestTool;

impl UpdatePullRequestTool {
    /// The stable dotted tool name.
    pub const NAME: &'static str = "github.update_pull_request";

    /// A GitHub write is a mutation, always approval-gated by the policy engine.
    pub fn proposed_action(repo: &RepoId) -> ProposedAction {
        github_mutation_action(repo, format!("update pull request on {}", repo.slug()))
    }
}

/// Parse `github.update_pull_request` arguments.
pub fn parse_update_pull_request(args: &Value) -> Result<UpdatePullRequestInput, String> {
    let number = args
        .get("number")
        .and_then(Value::as_u64)
        .ok_or("github.update_pull_request requires an integer `number`")?;
    let field = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
    Ok(UpdatePullRequestInput {
        number,
        request: UpdatePullRequest {
            title: field("title"),
            body: field("body"),
            state: field("state"),
        },
    })
}

/// The typed input for `github.create_check_run_summary`.
pub struct CreateCheckRunInput {
    /// The check-run request body.
    pub request: NewCheckRun,
    /// Derived replay-safety key (`head_sha:name`): the same summary name on
    /// the same commit is one logical write, however many times the model
    /// proposes it. Carried to GitHub as the check run's `external_id`.
    pub idempotency_key: String,
}

/// Post a check-run summary against a commit.
pub struct CreateCheckRunSummary;

impl CreateCheckRunSummary {
    /// The stable dotted tool name.
    pub const NAME: &'static str = "github.create_check_run_summary";

    /// A GitHub write is a mutation, always approval-gated by the policy engine.
    pub fn proposed_action(repo: &RepoId) -> ProposedAction {
        github_mutation_action(repo, format!("post check-run summary on {}", repo.slug()))
    }
}

/// Parse `github.create_check_run_summary` arguments.
pub fn parse_create_check_run(args: &Value) -> Result<CreateCheckRunInput, String> {
    let string = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("github.create_check_run_summary requires a string `{key}`"))
    };
    let name = string("name")?;
    let head_sha = string("head_sha")?;
    let idempotency_key = format!("{head_sha}:{name}");
    Ok(CreateCheckRunInput {
        request: NewCheckRun {
            name,
            head_sha,
            summary: string("summary")?,
            conclusion: args
                .get("conclusion")
                .and_then(Value::as_str)
                .map(str::to_string),
            external_id: None,
        },
        idempotency_key,
    })
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

/// The hard-coded `/fix-ci` objective (Phase 3 STEP 3.2). Registering `/fix-ci`
/// starts a `Build`-mode run with this objective in an isolated worktree on the
/// PR branch; the agent drives the Chapter 10 repair flow using the `github.*`
/// (read + write), `workspace.*`, `git.apply_patch`, and `shell.run` tools. Every
/// GitHub write is approval-gated by the policy engine, so the push and PR update
/// surface for approval before they happen. The declarative workflow engine
/// (Phase 5) later replaces this prompt-encoded sequence.
pub fn fix_ci_objective(repo_slug: &str, pr_number: u64) -> String {
    format!(
        "Repair the failing CI check on pull request #{pr_number} of {repo_slug}.\n\
         Work in this order, using only the provided tools:\n\
         1. `github.get_pull_request` to read PR #{pr_number} and its head branch.\n\
         2. `github.list_check_runs` on the head ref to find the failing check.\n\
         3. Investigate the failure with `workspace.search` / `workspace.read_file`.\n\
         4. Propose a fix with `git.apply_patch`.\n\
         5. Verify with `shell.run` (run the tests).\n\
         6. When the tests pass, `github.update_pull_request` to describe the fix \
            and `github.create_check_run_summary` to report the result. These are \
            writes — they will pause for your operator's approval.\n\
         Stop and summarize if you cannot make the tests pass."
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_ci_objective_names_the_pr_and_repo_and_the_write_tools() {
        let objective = fix_ci_objective("octocat/hello-world", 7);
        assert!(objective.contains("#7"));
        assert!(objective.contains("octocat/hello-world"));
        assert!(objective.contains("github.list_check_runs"));
        assert!(objective.contains("github.update_pull_request"));
        assert!(objective.contains("github.create_check_run_summary"));
        assert!(objective.contains("approval"));
    }

    #[test]
    fn create_draft_key_is_ref_safe() {
        let input = parse_create_draft_pull_request(
            &serde_json::json!({"title":"t","head":"h","base":"b"}),
        )
        .unwrap();
        assert_eq!(input.idempotency_key, "h:b");
    }

    #[test]
    fn update_pull_request_parses_optional_fields() {
        let input =
            parse_update_pull_request(&serde_json::json!({"number":3,"state":"closed"})).unwrap();
        assert_eq!(input.number, 3);
        assert_eq!(input.request.state.as_deref(), Some("closed"));
        assert!(input.request.title.is_none());
    }

    #[test]
    fn check_run_requires_its_fields() {
        assert!(parse_create_check_run(&serde_json::json!({"name":"ci"})).is_err());
        let ok = parse_create_check_run(
            &serde_json::json!({"name":"ci","head_sha":"abc","summary":"green"}),
        )
        .unwrap();
        assert_eq!(ok.request.head_sha, "abc");
        assert_eq!(ok.idempotency_key, "abc:ci");
        assert!(ok.request.external_id.is_none());
    }
}
