use super::{Search, SearchInput, SearchResults, MATCH_CAP, MAX_ERROR_BYTES};
use crate::tools::ToolError;
use codypendent_daemon::policy::PathScope;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

struct SearchFixture {
    root: tempfile::TempDir,
    executable: PathBuf,
}

impl SearchFixture {
    fn new(body: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("search-fixture");
        std::fs::write(root.path().join("source.rs"), "found the needle\n").unwrap();
        std::fs::write(
            &executable,
            format!("#!/bin/sh\nprintf x >> \"$0.calls\"\n{body}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { root, executable }
    }

    fn match_line(&self) -> String {
        serde_json::json!({"type":"match","data":{
            "path":{"text":self.root.path().canonicalize().unwrap().join("source.rs")},
            "line_number":7,"lines":{"text":"found the needle\n"}
        }})
        .to_string()
    }

    fn write_body(&self, body: &str) {
        std::fs::write(
            &self.executable,
            format!("#!/bin/sh\nprintf x >> \"$0.calls\"\n{body}\n"),
        )
        .unwrap();
    }

    fn attempts(&self) -> usize {
        std::fs::read(self.executable.with_extension("calls"))
            .unwrap()
            .len()
    }

    async fn search(&self, budget: Duration) -> Result<SearchResults, ToolError> {
        Search::execute_with(
            &SearchInput {
                pattern: "needle".into(),
                glob: None,
            },
            &PathScope::new(vec![self.root.path().canonicalize().unwrap()], vec![]),
            &self.executable,
            budget,
        )
        .await
    }
}

#[tokio::test]
async fn killed_search_retries_only_once_and_returns_the_successful_attempt() {
    let fixture = SearchFixture::new("");
    fixture.write_body(&format!(
        "if [ ! -f \"$0.killed\" ]; then : > \"$0.killed\"; printf '%s\\n' '{}'; kill -KILL $$; fi\nprintf '%s\\n' '{}'",
        fixture.match_line(),
        fixture.match_line(),
    ));
    let result = fixture.search(Duration::from_secs(2)).await.unwrap();
    assert_eq!(fixture.attempts(), 2);
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].line_number, 7);
    assert!(!result.truncated);
}

#[tokio::test]
async fn repeated_kills_surface_the_actual_failure_after_two_attempts() {
    let fixture = SearchFixture::new("kill -KILL $$");
    let error = fixture
        .search(Duration::from_secs(2))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(fixture.attempts(), 2);
    assert!(error.contains("SIGKILL"), "{error}");
    assert!(
        error.contains("Automatic retry attempted once (2 attempts total)"),
        "{error}"
    );
}

#[tokio::test]
async fn interrupted_exit_137_is_retried_but_other_failures_are_not() {
    let fixture = SearchFixture::new("exit 137");
    let error = fixture
        .search(Duration::from_secs(2))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(fixture.attempts(), 2);
    assert!(error.contains("137"), "{error}");
    assert!(error.contains("2 attempts total"), "{error}");

    for message in ["regex parse error", "file not found", "permission denied"] {
        let fixture = SearchFixture::new(&format!("printf '%s' '{message}' >&2\nexit 2"));
        let error = fixture
            .search(Duration::from_secs(2))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(fixture.attempts(), 1);
        assert!(error.contains(message), "{error}");
        assert!(error.contains('2'), "{error}");
        assert!(!error.contains("Automatic retry"), "{error}");
    }
}

#[tokio::test]
async fn a_failed_search_does_not_return_partial_matches_as_a_success() {
    let fixture = SearchFixture::new("");
    fixture.write_body(&format!(
        "printf '%s\\n' '{}'\nprintf 'unreadable directory' >&2\nexit 2",
        fixture.match_line(),
    ));
    let error = fixture
        .search(Duration::from_secs(2))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("unreadable directory"), "{error}");
    assert_eq!(fixture.attempts(), 1);
}

#[tokio::test]
async fn no_matches_is_a_success_without_a_retry() {
    let fixture = SearchFixture::new("exit 1");
    let result = fixture.search(Duration::from_secs(2)).await.unwrap();
    assert!(result.matches.is_empty());
    assert!(!result.truncated);
    assert_eq!(fixture.attempts(), 1);
}

#[tokio::test]
async fn intentional_match_cap_kills_are_successful_and_never_retried() {
    let fixture = SearchFixture::new("");
    fixture.write_body(&format!(
        "while :; do printf '%s\\n' '{}'; done",
        fixture.match_line()
    ));
    let result = fixture.search(Duration::from_secs(2)).await.unwrap();
    assert_eq!(result.matches.len(), MATCH_CAP);
    assert!(result.truncated);
    assert_eq!(fixture.attempts(), 1);
}

#[tokio::test]
async fn timed_out_search_gets_one_retry_inside_the_shared_deadline() {
    let fixture = SearchFixture::new("");
    fixture.write_body(&format!(
        "if [ ! -f \"$0.slept\" ]; then : > \"$0.slept\"; exec /bin/sleep 5; fi\nprintf '%s\\n' '{}'",
        fixture.match_line(),
    ));
    let result = fixture.search(Duration::from_secs(2)).await.unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(fixture.attempts(), 2);

    let fixture = SearchFixture::new("exec /bin/sleep 5");
    let started = std::time::Instant::now();
    let error = fixture
        .search(Duration::from_millis(800))
        .await
        .unwrap_err();
    let error = error.to_string();
    assert!(error.contains("workspace.search"), "{error}");
    assert!(error.contains("timed out"), "{error}");
    assert!(
        error.contains("Automatic retry attempted once (2 attempts total)"),
        "{error}"
    );
    assert_eq!(fixture.attempts(), 2);
    assert!(started.elapsed() < Duration::from_millis(1400));
}

#[tokio::test]
async fn stderr_is_drained_concurrently_and_kept_within_the_capture_bound() {
    let fixture = SearchFixture::new(
        "i=0; while [ $i -lt 2000 ]; do printf 'bounded-error-message' >&2; i=$((i+1)); done\nexit 2",
    );
    let error = fixture
        .search(Duration::from_secs(2))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("bounded-error-message"));
    assert!(error.ends_with('…'));
    assert!(error.len() < MAX_ERROR_BYTES + 100);
    assert_eq!(fixture.attempts(), 1);
}

#[tokio::test]
async fn retries_keep_the_original_pattern_glob_and_scope() {
    let fixture = SearchFixture::new("");
    fixture.write_body(&format!(
        "printf '%s\\n' \"$@\" > \"$0.args\"\nif [ ! -f \"$0.killed\" ]; then : > \"$0.killed\"; kill -KILL $$; fi\nprintf '%s\\n' '{}'",
        fixture.match_line(),
    ));
    let root = fixture.root.path().canonicalize().unwrap();
    let results = Search::execute_with(
        &SearchInput {
            pattern: "-literal needle".into(),
            glob: Some("*.rs".into()),
        },
        &PathScope::new(vec![root.clone()], vec![root.join("source.rs")]),
        &fixture.executable,
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert!(results.matches.is_empty());
    assert_eq!(fixture.attempts(), 2);
    assert_eq!(
        std::fs::read_to_string(fixture.executable.with_extension("args")).unwrap(),
        format!(
            "--json\n-n\n--no-config\n--glob\n*.rs\n--regexp\n-literal needle\n{}\n",
            root.display()
        ),
    );
}

#[tokio::test]
async fn cancelling_the_search_kills_its_child_without_retrying() {
    let fixture = SearchFixture::new("printf '%s' $$ > \"$0.pid\"\nexec /bin/sleep 5");
    let mut search = Box::pin(fixture.search(Duration::from_secs(2)));
    let pid_file = fixture.executable.with_extension("pid");
    let pid = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            tokio::select! {
                result = &mut search => panic!("search ended before cancellation: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {},
            }
            if let Ok(pid) = std::fs::read_to_string(&pid_file) {
                break rustix::process::Pid::from_raw(pid.parse().unwrap()).unwrap();
            }
        }
    })
    .await
    .unwrap();
    drop(search);
    tokio::time::timeout(Duration::from_secs(1), async {
        while rustix::process::test_kill_process(pid).is_ok() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fixture.attempts(), 1);
}
