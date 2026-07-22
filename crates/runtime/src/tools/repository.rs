//! `repository.test` — run the repository's own test command in the node's
//! worktree (Phase 5 T6).
//!
//! A workflow **tool node** whose action is `repository.test` verifies a change by
//! running the repository's test suite. It executes through the **same** sandboxed
//! process-spawn path as [`Shell`](super::Shell) — an empty environment, a `cwd`
//! confined to the granted path scope, an allow-listed program, a wall-clock
//! timeout, and full output spilled to the artifact store — rather than forking a
//! second spawn code path. Only the command *resolution* is specific to this tool:
//! a `.codypendent/test-command` repo override if present, else deterministic
//! detection from the build manifest. The resolved command is recorded in the
//! result so a downstream `test_result` blackboard artifact is self-describing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use codypendent_daemon::policy::{CommandScope, PathScope};
use codypendent_protocol::{ArtifactRef, RunId};

use super::{ArtifactSink, CommandRequest, Shell, ShellOutcome, ToolError};

/// The default per-run wall clock before the command scope's own ceiling clamps
/// it ([`Shell::execute`] applies the tighter of the two).
const DEFAULT_TEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The structured result of a `repository.test` run — the shape a `test_result`
/// blackboard artifact is built from: the resolved command, the exit status, and
/// a reference to the full captured output.
#[derive(Debug, Clone)]
pub struct RepositoryTestOutcome {
    /// The resolved command line that was run (e.g. `cargo test`), recorded so the
    /// result is self-describing regardless of how the command was resolved.
    pub command: String,
    /// The process exit code, or `None` if the process was killed (timeout/signal).
    pub exit_code: Option<i32>,
    /// Whether the command exited zero (the tests passed).
    pub success: bool,
    /// Whether the command was killed for exceeding its timeout.
    pub timed_out: bool,
    /// A reference to the full captured stdout in the artifact store, if any.
    pub output_ref: Option<ArtifactRef>,
    /// A one-line human summary of the outcome.
    pub summary: String,
}

/// The `repository.test` tool.
pub struct RepositoryTest;

impl RepositoryTest {
    /// The stable dotted tool name.
    pub const NAME: &'static str = "repository.test";

    /// Resolve the test command for `worktree`: a `.codypendent/test-command`
    /// override if present (the repo config surface, mirroring the `.codypendent/`
    /// precedent — `policy.toml`, `workflows/`), else deterministic detection by
    /// build manifest (`Cargo.toml` → `cargo test`, `package.json` → `npm test`,
    /// `pyproject.toml` → `pytest`). Returns the program + args, or a legible reason
    /// nothing could be resolved.
    pub async fn detect_command(worktree: &Path) -> Result<Vec<String>, String> {
        // Repo config surface: a `.codypendent/test-command` file, its contents a
        // whitespace-separated command line. Deliberately simple (no shell quoting);
        // a project needing more configures the command it wants to run.
        let configured = worktree.join(".codypendent").join("test-command");
        if let Ok(contents) = tokio::fs::read_to_string(&configured).await {
            let tokens: Vec<String> = contents.split_whitespace().map(str::to_string).collect();
            if !tokens.is_empty() {
                return Ok(tokens);
            }
        }
        if worktree.join("Cargo.toml").is_file() {
            return Ok(vec!["cargo".to_string(), "test".to_string()]);
        }
        if worktree.join("package.json").is_file() {
            return Ok(vec!["npm".to_string(), "test".to_string()]);
        }
        if worktree.join("pyproject.toml").is_file() {
            return Ok(vec!["pytest".to_string()]);
        }
        Err(format!(
            "no test command could be resolved for {} — add a `.codypendent/test-command`, \
             or a Cargo.toml / package.json / pyproject.toml",
            worktree.display()
        ))
    }

    /// Run `command` in `worktree` through [`Shell::execute`] (the shared sandboxed
    /// spawn path: empty environment, `cwd` confined to `path_scope`, program
    /// checked against `command_scope`, timeout, output spilled to `sink`).
    /// Returns the structured outcome; a refusal (program not allow-listed, `cwd`
    /// out of scope) surfaces as a [`ToolError`], exactly as for `shell.run`.
    pub async fn execute(
        command: &[String],
        worktree: &Path,
        path_scope: &PathScope,
        command_scope: &CommandScope,
        sink: &dyn ArtifactSink,
        run_id: RunId,
    ) -> Result<RepositoryTestOutcome, ToolError> {
        let (program, args) = command
            .split_first()
            .ok_or_else(|| ToolError::ProgramNotAllowed(String::new()))?;
        let request = CommandRequest {
            program: PathBuf::from(program),
            args: args.to_vec(),
            cwd: worktree.to_path_buf(),
            // The child inherits nothing (RULE 2c) — the test command runs in an
            // empty environment, exactly as `shell.run` does.
            environment: Vec::new(),
            timeout: DEFAULT_TEST_TIMEOUT,
        };
        let outcome: ShellOutcome =
            Shell::execute(&request, path_scope, command_scope, sink, run_id).await?;
        let display = command.join(" ");
        let summary = if outcome.timed_out {
            format!("`{display}` timed out")
        } else {
            match outcome.exit_code {
                Some(0) => format!("`{display}` passed"),
                Some(code) => format!("`{display}` failed (exit {code})"),
                None => format!("`{display}` was killed"),
            }
        };
        Ok(RepositoryTestOutcome {
            command: display,
            exit_code: outcome.exit_code,
            success: outcome.success(),
            timed_out: outcome.timed_out,
            output_ref: outcome.stdout_ref.clone(),
            summary,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[tokio::test]
    async fn detects_cargo_npm_and_pytest_by_manifest() {
        let cargo = tempdir().await;
        std::fs::write(cargo.path().join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(
            RepositoryTest::detect_command(cargo.path()).await.unwrap(),
            vec!["cargo".to_string(), "test".to_string()]
        );

        let node = tempdir().await;
        std::fs::write(node.path().join("package.json"), "{}\n").unwrap();
        assert_eq!(
            RepositoryTest::detect_command(node.path()).await.unwrap(),
            vec!["npm".to_string(), "test".to_string()]
        );

        let py = tempdir().await;
        std::fs::write(py.path().join("pyproject.toml"), "[project]\n").unwrap();
        assert_eq!(
            RepositoryTest::detect_command(py.path()).await.unwrap(),
            vec!["pytest".to_string()]
        );
    }

    #[tokio::test]
    async fn config_override_wins_over_detection() {
        let dir = tempdir().await;
        // A Cargo.toml is present, but the explicit override takes precedence.
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".codypendent")).unwrap();
        std::fs::write(
            dir.path().join(".codypendent").join("test-command"),
            "just test\n",
        )
        .unwrap();
        assert_eq!(
            RepositoryTest::detect_command(dir.path()).await.unwrap(),
            vec!["just".to_string(), "test".to_string()]
        );
    }

    #[tokio::test]
    async fn undetectable_repository_is_a_legible_error() {
        let dir = tempdir().await;
        let err = RepositoryTest::detect_command(dir.path())
            .await
            .unwrap_err();
        assert!(err.contains("no test command"), "legible reason: {err}");
    }
}
