//! `git.diff` and `git.apply_patch` — structured Git invocation over the granted
//! worktree. `apply_patch` runs `git apply --check` first and refuses on
//! failure; nothing is ever passed to a shell as an unparsed string.
//!
//! These are trusted, daemon-issued invocations of `git` against the run's own
//! worktree (not model-proposed programs), so — unlike [`shell.run`](super::Shell)
//! — they run with the daemon's environment. The capability check they still
//! enforce is the one the tool table names: the worktree `cwd` must be inside
//! the granted path scope and `git` must be in the command allow-list.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use codypendent_daemon::artifacts::Provenance;
use codypendent_daemon::policy::{CommandScope, PathScope, ScopeVerdict};
use codypendent_protocol::{ArtifactRef, ProposedAction, RunId};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::{ArtifactSink, CapabilityKind, ToolError, IN_MEMORY_CAP};

/// The program both Git tools require in the allow-list.
const GIT: &str = "git";

/// Wall-clock bound on one git invocation. These are local worktree operations
/// (`diff`, `apply`) that finish in seconds; without a bound a wedged git (lock
/// contention, a filter/smudge process gone wrong) blocks the run forever —
/// cancellation is blind while a tool executes. Both commands set
/// `kill_on_drop`, so expiring the timeout (dropping the wait future) kills the
/// child.
const GIT_TIMEOUT_SECS: u64 = 300;

/// Await a git wait-future under [`GIT_TIMEOUT_SECS`], surfacing expiry as a
/// structured [`ToolError::TimedOut`] for `tool`.
async fn bounded_git<T>(
    tool: &'static str,
    wait: impl std::future::Future<Output = std::io::Result<T>>,
) -> Result<T, ToolError> {
    match tokio::time::timeout(Duration::from_secs(GIT_TIMEOUT_SECS), wait).await {
        Ok(result) => result.map_err(ToolError::Io),
        Err(_elapsed) => Err(ToolError::TimedOut {
            tool,
            seconds: GIT_TIMEOUT_SECS,
        }),
    }
}

/// Guard shared by both Git tools: `git` allow-listed and `cwd` in scope.
fn guard(
    cwd: &std::path::Path,
    path_scope: &PathScope,
    command_scope: &CommandScope,
) -> Result<(), ToolError> {
    if !command_scope.allows_program(GIT) {
        return Err(ToolError::ProgramNotAllowed(GIT.to_string()));
    }
    match path_scope.classify(cwd) {
        ScopeVerdict::Allowed => Ok(()),
        ScopeVerdict::Denied => Err(ToolError::PathDenied(cwd.to_path_buf())),
        ScopeVerdict::OutsideRoots => Err(ToolError::CwdOutOfScope(cwd.to_path_buf())),
    }
}

/// Typed input for [`GitDiff::execute`].
#[derive(Debug, Clone)]
pub struct GitDiffInput {
    /// The worktree to diff.
    pub cwd: PathBuf,
}

/// The result of `git.diff`.
#[derive(Debug, Clone)]
pub struct GitDiffOutcome {
    /// Whether the worktree has no unstaged changes.
    pub is_empty: bool,
    /// The diff text (truncated to the in-memory cap; see `truncated`).
    pub diff: String,
    /// Whether `diff` was truncated relative to the full spilled artifact.
    pub truncated: bool,
    /// The full diff spilled to the store, if non-empty.
    pub artifact: Option<ArtifactRef>,
}

/// The `git.diff` tool (worktree diff).
pub struct GitDiff;

impl GitDiff {
    /// The stable tool name.
    pub const NAME: &'static str = "git.diff";

    /// Capability classes this tool draws on (per the STEP 1.7 tool table).
    pub fn required_capabilities() -> &'static [CapabilityKind] {
        &[CapabilityKind::FileWrite, CapabilityKind::CommandExecute]
    }

    /// The [`ProposedAction`] the middleware evaluates before granting. It mirrors
    /// the actual invocation `git -C <cwd> --no-pager diff` so policy, approval,
    /// and audit see the real command (the `cwd` is `-C <dir>`, not a pathspec).
    pub fn proposed_action(input: &GitDiffInput) -> ProposedAction {
        ProposedAction::ExecuteCommand {
            program: GIT.to_string(),
            args: vec![
                "-C".to_string(),
                input.cwd.to_string_lossy().into_owned(),
                "--no-pager".to_string(),
                "diff".to_string(),
            ],
            // git is a trusted daemon-issued invocation with no model-supplied
            // bindings (its interposition vars are stripped at spawn time).
            environment: Vec::new(),
            cwd: Some(input.cwd.to_string_lossy().into_owned()),
        }
    }

    /// Produce the worktree diff, spilling the full text to `sink`.
    pub async fn execute(
        input: &GitDiffInput,
        path_scope: &PathScope,
        command_scope: &CommandScope,
        sink: &dyn ArtifactSink,
        run_id: RunId,
    ) -> Result<GitDiffOutcome, ToolError> {
        guard(&input.cwd, path_scope, command_scope)?;

        let mut command = Command::new(GIT);
        command
            .arg("-C")
            .arg(&input.cwd)
            .arg("--no-pager")
            .arg("diff")
            .current_dir(&input.cwd)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        harden_git_env(&mut command);
        let output = bounded_git(Self::NAME, command.output())
            .await
            .map_err(|e| match e {
                ToolError::Io(io) => map_spawn(io),
                other => other,
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(ToolError::Other(anyhow::anyhow!(
                "git diff failed: {}",
                stderr.trim()
            )));
        }

        let is_empty = output.stdout.is_empty();
        let artifact = if is_empty {
            None
        } else {
            let provenance = Provenance::tool_output(Self::NAME, run_id);
            Some(
                sink.store("text/x-diff", provenance, &output.stdout)
                    .await
                    .map_err(ToolError::Other)?,
            )
        };

        let full = String::from_utf8_lossy(&output.stdout);
        let truncated = full.len() > IN_MEMORY_CAP;
        let diff = if truncated {
            let mut end = IN_MEMORY_CAP;
            while end > 0 && !full.is_char_boundary(end) {
                end -= 1;
            }
            full[..end].to_string()
        } else {
            full.into_owned()
        };

        Ok(GitDiffOutcome {
            is_empty,
            diff,
            truncated,
            artifact,
        })
    }
}

/// Typed input for [`ApplyPatch::execute`].
#[derive(Debug, Clone)]
pub struct ApplyPatchInput {
    /// The worktree to apply into.
    pub cwd: PathBuf,
    /// The unified-diff patch text.
    pub patch: String,
}

/// The result of a successful `git.apply_patch`.
#[derive(Debug, Clone)]
pub struct ApplyPatchOutcome {
    /// Always `true` on success (the error path carries the refusal).
    pub applied: bool,
}

/// The `git.apply_patch` tool.
pub struct ApplyPatch;

impl ApplyPatch {
    /// The stable tool name.
    pub const NAME: &'static str = "git.apply_patch";

    /// Capability classes this tool draws on (per the STEP 1.7 tool table).
    pub fn required_capabilities() -> &'static [CapabilityKind] {
        &[CapabilityKind::FileWrite, CapabilityKind::CommandExecute]
    }

    /// The [`ProposedAction`] the middleware evaluates before granting. It mirrors
    /// the actual invocation `git -C <cwd> apply` (the patch text arrives on
    /// stdin) so policy, approval, and audit see the real command.
    pub fn proposed_action(input: &ApplyPatchInput) -> ProposedAction {
        ProposedAction::ExecuteCommand {
            program: GIT.to_string(),
            args: vec![
                "-C".to_string(),
                input.cwd.to_string_lossy().into_owned(),
                "apply".to_string(),
            ],
            environment: Vec::new(),
            cwd: Some(input.cwd.to_string_lossy().into_owned()),
        }
    }

    /// Apply `input.patch` to the worktree. Runs `git apply --check` first and
    /// refuses (without mutating anything) if the patch does not apply cleanly.
    pub async fn execute(
        input: &ApplyPatchInput,
        path_scope: &PathScope,
        command_scope: &CommandScope,
    ) -> Result<ApplyPatchOutcome, ToolError> {
        guard(&input.cwd, path_scope, command_scope)?;

        // Dry run first — refuse before touching the worktree.
        let check = run_git_apply(&input.cwd, &["apply", "--check"], &input.patch).await?;
        if !check.status.success() {
            let stderr = String::from_utf8_lossy(&check.stderr).into_owned();
            return Err(ToolError::PatchDoesNotApply(stderr.trim().to_string()));
        }

        // Real apply.
        let applied = run_git_apply(&input.cwd, &["apply"], &input.patch).await?;
        if !applied.status.success() {
            let stderr = String::from_utf8_lossy(&applied.stderr).into_owned();
            return Err(ToolError::PatchDoesNotApply(stderr.trim().to_string()));
        }

        Ok(ApplyPatchOutcome { applied: true })
    }
}

/// Run `git -C <cwd> <args…>` feeding `patch` on stdin, capturing output.
async fn run_git_apply(
    cwd: &std::path::Path,
    args: &[&str],
    patch: &str,
) -> Result<std::process::Output, ToolError> {
    let mut command = Command::new(GIT);
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    harden_git_env(&mut command);
    let mut child = command.spawn().map_err(map_spawn)?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ToolError::Other(anyhow::anyhow!("git stdin unavailable")))?;
        stdin.write_all(patch.as_bytes()).await?;
        stdin.shutdown().await?;
    }

    bounded_git(ApplyPatch::NAME, child.wait_with_output()).await
}

/// Strip ambient git interposition variables (a repo config or the daemon's own
/// environment could set these to run an arbitrary program during `diff`/`apply`)
/// and disable credential prompting. Unlike `shell.run`, git legitimately
/// inherits the daemon environment as a trusted, daemon-issued invocation, so
/// this removes only the known execution hooks rather than clearing wholesale.
fn harden_git_env(command: &mut Command) {
    for key in [
        "GIT_EXTERNAL_DIFF",
        "GIT_SSH_COMMAND",
        "GIT_SSH",
        "GIT_PROXY_COMMAND",
        "GIT_PAGER",
        "GIT_EDITOR",
        "GIT_ASKPASS",
        // The whole injected-config family: the deprecated GIT_CONFIG plus the
        // modern GIT_CONFIG_GLOBAL/SYSTEM overrides and GIT_CONFIG_COUNT/
        // KEY_n/VALUE_n parameter injection — any of these can point diff/apply
        // at config that runs an arbitrary program (core.fsmonitor, filters).
        "GIT_CONFIG",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_PARAMETERS",
    ] {
        command.env_remove(key);
    }
    // GIT_CONFIG_COUNT gates the numbered KEY_n/VALUE_n pairs; removing the
    // count disables the whole set without enumerating n.
    command.env_remove("GIT_CONFIG_COUNT");
    command.env("GIT_TERMINAL_PROMPT", "0");
}

/// Map a spawn failure, distinguishing a missing `git` binary.
fn map_spawn(e: std::io::Error) -> ToolError {
    if e.kind() == std::io::ErrorKind::NotFound {
        ToolError::ProgramNotFound(GIT.to_string())
    } else {
        ToolError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::GIT_TIMEOUT_SECS;
    use crate::tools::ABSOLUTE_MAX_TIMEOUT;

    /// `git.diff` and `git.apply_patch` both run under [`GIT_TIMEOUT_SECS`] via
    /// `bounded_git` (C12: these tools previously had no timeout at all). Pin —
    /// at compile time — that the bound is a real, finite ceiling within the
    /// runtime's absolute wall-clock maximum, so neither can hang the agent loop
    /// forever.
    #[test]
    fn git_tool_timeout_is_bounded() {
        const { assert!(GIT_TIMEOUT_SECS > 0) };
        const { assert!(GIT_TIMEOUT_SECS <= ABSOLUTE_MAX_TIMEOUT.as_secs()) };
    }
}
