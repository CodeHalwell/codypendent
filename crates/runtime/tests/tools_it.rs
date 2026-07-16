//! STEP 1.7 tool-layer integration tests.
//!
//! Grants ([`PathScope`]/[`CommandScope`]) are built directly through the policy
//! API; real temp directories and a real repo exercise the tools; a real
//! [`ArtifactStore`] backs the output spill via a [`ClosureSink`] (which lets
//! the test capture a SQLite pool value it cannot name — see the tool module
//! docs). Covers the five non-negotiable tests plus happy-path coverage of the
//! read/search/git tools.

use std::path::Path;
use std::time::Duration;

use codypendent_daemon::artifacts::{ArtifactStore, Provenance};
use codypendent_daemon::db::open_database;
use codypendent_daemon::policy::{CommandScope, PathScope};
use codypendent_protocol::{ArtifactRef, DataClassification, RunId};
use codypendent_runtime::tools::{
    ApplyPatch, ApplyPatchInput, ArtifactSink, ClosureSink, CommandRequest, EnvironmentBinding,
    GitDiff, GitDiffInput, ReadFile, ReadFileInput, Search, SearchInput, Shell, ToolError,
};
use tokio::io::AsyncReadExt;

/// Build an [`ArtifactSink`] over an `ArtifactStore` + pool by capturing clones.
/// A macro (not a function) so the closure body sees the pool's concrete type,
/// which cannot be named in this crate.
macro_rules! store_sink {
    ($store:expr, $pool:expr) => {{
        let store = $store.clone();
        let pool = $pool.clone();
        ClosureSink(move |media: String, prov: Provenance, bytes: Vec<u8>| {
            let store = store.clone();
            let pool = pool.clone();
            async move {
                store
                    .put(&pool, &media, DataClassification::Internal, prov, &bytes)
                    .await
            }
        })
    }};
}

/// A canonical single-root path scope.
fn canon_scope(root: &Path) -> PathScope {
    PathScope::new(vec![std::fs::canonicalize(root).unwrap()], vec![])
}

/// A command scope with the given allow-list and no wall-clock ceiling.
fn cmd_scope(programs: &[&str]) -> CommandScope {
    CommandScope {
        allowed_programs: programs.iter().map(|s| s.to_string()).collect(),
        maximum_seconds: 0,
    }
}

/// A sink that fails if used — for tests where no output should be spilled.
fn refusing_sink() -> impl ArtifactSink {
    ClosureSink(|_m: String, _p: Provenance, _b: Vec<u8>| async move {
        Err::<ArtifactRef, _>(anyhow::anyhow!("artifact sink must not be called"))
    })
}

// ---------------------------------------------------------------------------
// shell.run — the five required behaviours (rules 1–4).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shell_run_rejects_non_allowlisted_program() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    let request = CommandRequest {
        program: "rm".into(),
        args: vec!["-rf".into(), "/".into()],
        cwd,
        environment: vec![],
        timeout: Duration::from_secs(5),
    };
    let sink = refusing_sink();
    // `rm` is not in the allow-list → structured refusal, and (because the check
    // precedes the spawn) no process is ever created.
    let err = Shell::execute(
        &request,
        &canon_scope(dir.path()),
        &cmd_scope(&["cargo", "git"]),
        &sink,
        RunId::new(),
    )
    .await
    .expect_err("non-allow-listed program must be refused");
    assert!(matches!(err, ToolError::ProgramNotAllowed(_)));
    assert_eq!(err.code(), "tool.program-not-allowlisted");
}

#[tokio::test]
async fn shell_run_timeout_kills_process() {
    // The whole test is wrapped in a hard timeout so a kill bug cannot hang CI.
    tokio::time::timeout(Duration::from_secs(20), async {
        let dir = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(dir.path()).unwrap();
        let request = CommandRequest {
            program: "sleep".into(),
            args: vec!["30".into()],
            cwd,
            environment: vec![],
            timeout: Duration::from_millis(300),
        };
        let sink = refusing_sink();
        let outcome = Shell::execute(
            &request,
            &canon_scope(dir.path()),
            &cmd_scope(&["sleep"]),
            &sink,
            RunId::new(),
        )
        .await
        .expect("a killed command still yields an outcome");
        assert!(outcome.timed_out, "must be flagged as timed out");
        assert!(!outcome.success(), "a killed run is not a success");
        assert_eq!(outcome.exit_code, None, "a killed process has no exit code");
    })
    .await
    .expect("the test itself must not hang — the child kill must terminate");
}

#[tokio::test]
async fn shell_run_env_isolation_hides_daemon_canary() {
    std::env::set_var("CODYPENDENT_CANARY", "topsecret-value");
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path().join("artifacts"));
    let pool = open_database(&dir.path().join("test.db")).await.unwrap();
    let sink = store_sink!(store, pool);

    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    let request = CommandRequest {
        program: "env".into(),
        args: vec![],
        cwd,
        environment: vec![EnvironmentBinding::new("SAFE_VAR", "safe-value")],
        timeout: Duration::from_secs(10),
    };
    let outcome = Shell::execute(
        &request,
        &canon_scope(dir.path()),
        &cmd_scope(&["env"]),
        &sink,
        RunId::new(),
    )
    .await
    .unwrap();
    assert!(outcome.success());

    let stdout_ref = outcome
        .stdout_ref
        .clone()
        .expect("env prints its environment");
    let mut file = store.open(&pool, stdout_ref.id).await.unwrap();
    let mut captured = String::new();
    file.read_to_string(&mut captured).await.unwrap();

    // The explicit binding is present; the daemon's canary is not — the child
    // environment started empty (RULE 2).
    assert!(
        captured.contains("SAFE_VAR=safe-value"),
        "explicit binding must pass through"
    );
    assert!(
        !captured.contains("CODYPENDENT_CANARY"),
        "canary name leaked into the child"
    );
    assert!(
        !captured.contains("topsecret-value"),
        "canary value leaked into the child"
    );
    assert!(!outcome.salient.render().contains("CODYPENDENT_CANARY"));
}

#[tokio::test]
async fn shell_run_output_cap_spills_to_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path().join("artifacts"));
    let pool = open_database(&dir.path().join("test.db")).await.unwrap();
    let sink = store_sink!(store, pool);

    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    // `seq 1 300000` is ~1.9 MiB — over the 1 MiB in-memory soft cap.
    let request = CommandRequest {
        program: "seq".into(),
        args: vec!["1".into(), "300000".into()],
        cwd,
        environment: vec![],
        timeout: Duration::from_secs(30),
    };
    let outcome = Shell::execute(
        &request,
        &canon_scope(dir.path()),
        &cmd_scope(&["seq"]),
        &sink,
        RunId::new(),
    )
    .await
    .unwrap();
    assert!(outcome.success());

    // Full output was spilled to the store...
    let stdout_ref = outcome.stdout_ref.clone().expect("large output must spill");
    assert!(
        stdout_ref.byte_length > 1_048_576,
        "artifact holds the full >1MiB output, got {} bytes",
        stdout_ref.byte_length
    );

    // ...and the salient view is truncated but references it.
    let stdout = &outcome.salient.stdout;
    assert_eq!(stdout.total_lines, 300_000);
    assert!(stdout.truncated, "salient must be truncated");
    assert!(stdout.large, "output must be flagged large");
    assert!(
        stdout.lines.len() < 200,
        "salient keeps only head/tail/error lines"
    );
    assert!(stdout.artifact.is_some(), "salient references the artifact");

    // The stored blob really is the full output.
    let mut file = store.open(&pool, stdout_ref.id).await.unwrap();
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await.unwrap();
    assert_eq!(buf.len() as u64, stdout_ref.byte_length);
    assert!(
        buf.len() > outcome.salient.render().len(),
        "the artifact is far larger than the compacted view"
    );
}

// ---------------------------------------------------------------------------
// workspace.read_file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_file_returns_line_numbered_excerpt() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(root.join("f.txt"), "alpha\nbeta\ngamma\n").unwrap();
    let excerpt = ReadFile::execute(
        &ReadFileInput {
            path: root.join("f.txt"),
            range: None,
        },
        &PathScope::new(vec![root.clone()], vec![]),
    )
    .await
    .unwrap();
    assert_eq!(excerpt.total_lines, 3);
    assert!(excerpt.content.contains("     1\talpha"));
    assert!(excerpt.content.contains("     3\tgamma"));
    assert!(!excerpt.truncated);
}

#[tokio::test]
async fn read_file_refuses_out_of_scope_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir(root.join("inside")).unwrap();
    std::fs::write(root.join("inside/ok.txt"), "ok\n").unwrap();
    std::fs::write(root.join("secret.txt"), "secret\n").unwrap();
    let scope = PathScope::new(vec![root.join("inside")], vec![]);

    // In-scope read succeeds.
    ReadFile::execute(
        &ReadFileInput {
            path: root.join("inside/ok.txt"),
            range: None,
        },
        &scope,
    )
    .await
    .unwrap();

    // A sibling outside the granted root is refused.
    let err = ReadFile::execute(
        &ReadFileInput {
            path: root.join("secret.txt"),
            range: None,
        },
        &scope,
    )
    .await
    .expect_err("out-of-scope read must be refused");
    assert!(matches!(err, ToolError::PathOutOfScope(_)));
    assert_eq!(err.code(), "tool.path-out-of-scope");
}

// ---------------------------------------------------------------------------
// workspace.search
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_finds_matches_confined_to_scope() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(root.join("a.rs"), "fn foo() {}\nlet x = 1;\n").unwrap();
    std::fs::write(root.join("b.rs"), "fn bar() {}\n").unwrap();
    let results = Search::execute(
        &SearchInput {
            pattern: "fn ".into(),
            glob: Some("*.rs".into()),
        },
        &PathScope::new(vec![root.clone()], vec![]),
    )
    .await
    .unwrap();
    assert!(
        results.matches.len() >= 2,
        "expected fn matches, got {:?}",
        results.matches
    );
    assert!(results.matches.iter().all(|m| m.line.contains("fn")));
    assert!(!results.truncated);
}

// ---------------------------------------------------------------------------
// git.diff / git.apply_patch
// ---------------------------------------------------------------------------

async fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .expect("git runs")
}

async fn init_repo(dir: &Path) {
    assert!(git(dir, &["init", "-q"]).await.status.success());
    git(dir, &["config", "user.email", "t@example.com"]).await;
    git(dir, &["config", "user.name", "Test"]).await;
    git(dir, &["config", "commit.gpgsign", "false"]).await;
}

#[tokio::test]
async fn git_diff_then_apply_patch_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(dir.path()).unwrap();
    init_repo(&repo).await;
    std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
    assert!(git(&repo, &["add", "."]).await.status.success());
    assert!(git(&repo, &["commit", "-q", "-m", "init"])
        .await
        .status
        .success());

    // Modify, then produce the worktree diff.
    std::fs::write(repo.join("a.txt"), "goodbye\n").unwrap();
    let store = ArtifactStore::new(dir.path().join("artifacts"));
    let pool = open_database(&dir.path().join("test.db")).await.unwrap();
    let sink = store_sink!(store, pool);
    let path_scope = PathScope::new(vec![repo.clone()], vec![]);
    let command_scope = cmd_scope(&["git"]);

    let diff = GitDiff::execute(
        &GitDiffInput { cwd: repo.clone() },
        &path_scope,
        &command_scope,
        &sink,
        RunId::new(),
    )
    .await
    .unwrap();
    assert!(!diff.is_empty);
    assert!(diff.diff.contains("goodbye"), "diff shows the change");
    assert!(diff.artifact.is_some(), "full diff spilled to the store");
    let patch = diff.diff.clone();

    // Revert the worktree, then re-apply the captured patch.
    assert!(git(&repo, &["checkout", "--", "a.txt"])
        .await
        .status
        .success());
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "hello\n"
    );

    ApplyPatch::execute(
        &ApplyPatchInput {
            cwd: repo.clone(),
            patch,
        },
        &path_scope,
        &command_scope,
    )
    .await
    .expect("a valid patch applies");
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "goodbye\n"
    );

    // A bogus patch is refused by the `git apply --check` pre-flight.
    let err = ApplyPatch::execute(
        &ApplyPatchInput {
            cwd: repo.clone(),
            patch: "this is not a patch\n".into(),
        },
        &path_scope,
        &command_scope,
    )
    .await
    .expect_err("an invalid patch must be refused");
    assert!(matches!(err, ToolError::PatchDoesNotApply(_)));
    assert_eq!(err.code(), "tool.patch-does-not-apply");
}
