//! Integration tests for the personal-mode GitHub client (Phase 3 STEP 3.1),
//! driven against a `wiremock` mock of the GitHub REST API.
//!
//! The focus is the behavior that is hard to get right and easy to regress:
//! idempotent creates (list-before-create finds a prior object by its hidden
//! marker), the token never entering any serializable/loggable surface, and
//! non-2xx responses mapping to a typed API error.

use codypendent_integrations::github::idempotency;
use codypendent_integrations::github::model::{NewCheckRun, NewPullRequest, UpdatePullRequest};
use codypendent_integrations::github::{
    GitHubApi, GitHubError, GitHubToken, RepoId, RestGitHubClient,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SENTINEL_TOKEN: &str = "ghp_SEKRET";

fn client(server: &MockServer) -> RestGitHubClient {
    RestGitHubClient::new(server.uri(), GitHubToken::new(SENTINEL_TOKEN)).expect("build client")
}

fn pr_json(number: u64, body: &str) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "title": "Add feature",
        "body": body,
        "state": "open",
        "draft": true,
        "html_url": "https://example.test/pull/1",
        "head": { "ref": "feature" },
        "base": { "ref": "main" }
    })
}

fn comment_json(id: u64, body: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "body": body,
        "user": { "login": "octocat" }
    })
}

#[tokio::test]
async fn idempotent_draft_pr() {
    let server = MockServer::start().await;
    let repo = RepoId::new("o", "r");
    let key = "draft-pr-idem-key";
    let created_body = idempotency::body_with_marker("draft pr", key);

    // First list returns []: nothing exists yet, so the client creates.
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;

    // Subsequent lists return the created PR, marker and all.
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!([pr_json(1, &created_body)])),
        )
        .with_priority(2)
        .mount(&server)
        .await;

    // The create endpoint must be hit at most once across both calls.
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls"))
        .respond_with(ResponseTemplate::new(201).set_body_json(pr_json(1, &created_body)))
        .expect(1)
        .mount(&server)
        .await;

    let gh = client(&server);
    let req = NewPullRequest::draft("Add feature", "feature", "main");

    let first = gh
        .create_draft_pull_request(&repo, &req, key)
        .await
        .expect("first create");
    let second = gh
        .create_draft_pull_request(&repo, &req, key)
        .await
        .expect("second create");

    assert_eq!(first.number, 1);
    assert_eq!(second.number, 1, "retry must find the existing PR");
    // `.expect(1)` on the POST mock is verified when `server` drops.
}

#[tokio::test]
async fn create_review_comment_is_idempotent() {
    let server = MockServer::start().await;
    let repo = RepoId::new("o", "r");
    let key = "comment-idem-key";
    let created_body = idempotency::body_with_marker("please fix", key);

    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues/1/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues/1/comments"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!([comment_json(7, &created_body)])),
        )
        .with_priority(2)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/1/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(comment_json(7, &created_body)))
        .expect(1)
        .mount(&server)
        .await;

    let gh = client(&server);

    let first = gh
        .create_review_comment(&repo, 1, "please fix", key)
        .await
        .expect("first comment");
    let second = gh
        .create_review_comment(&repo, 1, "please fix", key)
        .await
        .expect("second comment");

    assert_eq!(first.id, 7);
    assert_eq!(second.id, 7, "retry must find the existing comment");
}

#[test]
fn token_never_serialized() {
    // The token's Debug is redacted.
    let token = GitHubToken::new(SENTINEL_TOKEN);
    let debug = format!("{token:?}");
    assert!(
        !debug.contains(SENTINEL_TOKEN),
        "Debug leaked the token: {debug}"
    );

    // No serializable model type carries the token: serializing request bodies
    // can never surface it, because it is not a field of any of them.
    let new_pr = NewPullRequest::draft("t", "feature", "main");
    let update = UpdatePullRequest {
        title: Some("t".to_string()),
        body: Some("b".to_string()),
        state: Some("open".to_string()),
    };
    let check = NewCheckRun {
        name: "ci".to_string(),
        head_sha: "abc123".to_string(),
        summary: "ok".to_string(),
        conclusion: Some("success".to_string()),
    };
    for value in [
        serde_json::to_string(&new_pr).expect("serialize new pr"),
        serde_json::to_string(&update).expect("serialize update"),
        serde_json::to_string(&check).expect("serialize check"),
    ] {
        assert!(
            !value.contains(SENTINEL_TOKEN),
            "a model serialization contained the token: {value}"
        );
    }
}

#[tokio::test]
async fn http_error_maps() {
    let server = MockServer::start().await;
    let repo = RepoId::new("o", "r");

    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls/999"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
        .mount(&server)
        .await;

    let gh = client(&server);
    let err = gh
        .get_pull_request(&repo, 999)
        .await
        .expect_err("404 must be an error");

    match err {
        GitHubError::Api { status, .. } => assert_eq!(status, 404),
        other => panic!("expected GitHubError::Api {{ status: 404 }}, got {other:?}"),
    }
}

#[tokio::test]
async fn hostile_path_parameters_are_refused_before_any_request() {
    // No mocks mounted: a request reaching the server would 404 into
    // GitHubError::Api. The refusal must be InvalidParameter, proving the
    // request was never built — a traversal ref like this would otherwise
    // normalize into a DIFFERENT api.github.com endpoint under the user's token.
    let server = MockServer::start().await;
    let gh = client(&server);

    let traversal = RepoId::new("o", "r");
    let err = gh
        .list_check_runs(&traversal, "x/../../../../repos/evil/evil/issues")
        .await
        .expect_err("a traversal ref must be refused");
    assert!(matches!(
        err,
        GitHubError::InvalidParameter { kind: "ref", .. }
    ));

    let bad_owner = RepoId::new("o/../evil", "r");
    let err = gh
        .get_pull_request(&bad_owner, 1)
        .await
        .expect_err("a slash-bearing owner must be refused");
    assert!(matches!(
        err,
        GitHubError::InvalidParameter { kind: "owner", .. }
    ));

    let query_ref = RepoId::new("o", "r");
    let err = gh
        .list_check_runs(&query_ref, "HEAD?per_page=1")
        .await
        .expect_err("a query-injecting ref must be refused");
    assert!(matches!(
        err,
        GitHubError::InvalidParameter { kind: "ref", .. }
    ));

    // Ordinary parameters still pass validation (the call then 404s at the
    // mockless server, proving the request WAS built and sent).
    let err = gh
        .list_check_runs(&query_ref, "feature/branch-1.2")
        .await
        .expect_err("no mock mounted");
    assert!(matches!(err, GitHubError::Api { .. }));
}
