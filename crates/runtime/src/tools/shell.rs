//! `shell.run` — structured, sandboxed command execution.
//!
//! Enforces the Chapter 11 / STEP 1.7 rules: a structured request (never an
//! unparsed shell string), an allow-listed program, a `cwd` inside the granted
//! path scope, an environment that starts *empty* plus only explicit bindings
//! (no inherited secrets), a timeout that kills the process group, an output cap
//! that spills to the artifact store, and an observation-compacted salient view
//! as the only thing the model reads.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use codypendent_daemon::artifacts::Provenance;
use codypendent_daemon::policy::{CommandScope, PathScope, ScopeVerdict};
use codypendent_protocol::{ArtifactRef, ProposedAction, RunId};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use super::{
    display_command, effective_timeout, salient, ArtifactSink, CapabilityKind, ToolError,
    MAX_CAPTURE_BYTES,
};

/// One name/value pair permitted into the child environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentBinding {
    /// The variable name.
    pub name: String,
    /// The variable value.
    pub value: String,
}

impl EnvironmentBinding {
    /// Convenience constructor.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// A structured command request (Chapter 11). There is deliberately no field for
/// an unparsed shell string.
#[derive(Debug, Clone)]
pub struct CommandRequest {
    /// The program to run (bare name resolved on the daemon PATH, or a path).
    pub program: PathBuf,
    /// Arguments, passed verbatim — never re-parsed by a shell.
    pub args: Vec<String>,
    /// Working directory; must be inside the granted path scope.
    pub cwd: PathBuf,
    /// The *complete* child environment — the process inherits nothing else.
    pub environment: Vec<EnvironmentBinding>,
    /// Wall-clock timeout; clamped down to the command scope's ceiling.
    pub timeout: Duration,
}

/// The result of running a command. `exit_code` is `None` when the process was
/// killed (timeout or signal); `salient` is the observation-compacted view and
/// the `*_ref` fields point at the full spilled output.
#[derive(Debug, Clone)]
pub struct ShellOutcome {
    /// Process exit code, or `None` if killed.
    pub exit_code: Option<i32>,
    /// Whether the command was killed for exceeding its timeout.
    pub timed_out: bool,
    /// Wall-clock duration.
    pub duration: Duration,
    /// Full standard output, if it was spilled to the store.
    pub stdout_ref: Option<ArtifactRef>,
    /// Full standard error, if it was spilled to the store.
    pub stderr_ref: Option<ArtifactRef>,
    /// The model-facing compacted view.
    pub salient: salient::SalientView,
}

impl ShellOutcome {
    /// Whether the command completed with a zero exit code.
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// The `shell.run` tool.
pub struct Shell;

impl Shell {
    /// The stable tool name.
    pub const NAME: &'static str = "shell.run";

    /// Capability classes this tool draws on.
    pub fn required_capabilities() -> &'static [CapabilityKind] {
        &[CapabilityKind::CommandExecute]
    }

    /// The [`ProposedAction`] the middleware evaluates before granting. The full
    /// child environment and `cwd` are carried on the action so the approver and
    /// the audit ledger see exactly what the command will run with — not just its
    /// program and args.
    pub fn proposed_action(request: &CommandRequest) -> ProposedAction {
        ProposedAction::ExecuteCommand {
            program: request.program.to_string_lossy().into_owned(),
            args: request.args.clone(),
            environment: request
                .environment
                .iter()
                .map(|b| (b.name.clone(), b.value.clone()))
                .collect(),
            cwd: Some(request.cwd.to_string_lossy().into_owned()),
        }
    }

    /// Run `request` under the granted scopes, spilling full output to `sink`.
    ///
    /// Refuses — with no process spawned — a program not in `command_scope` or a
    /// `cwd` outside `path_scope`. A timeout is a *successful* non-zero outcome,
    /// not an error.
    pub async fn execute(
        request: &CommandRequest,
        path_scope: &PathScope,
        command_scope: &CommandScope,
        sink: &dyn ArtifactSink,
        run_id: RunId,
    ) -> Result<ShellOutcome, ToolError> {
        // RULE 2a: the program must be allow-listed. Check before anything is
        // spawned so a rejected program leaves no trace.
        let program_str = request.program.to_string_lossy().into_owned();
        if !command_scope.allows_program(&program_str) {
            return Err(ToolError::ProgramNotAllowed(program_str));
        }

        // RULE 2b: the working directory must be inside the granted scope.
        match path_scope.classify(&request.cwd) {
            ScopeVerdict::Allowed => {}
            ScopeVerdict::Denied => return Err(ToolError::PathDenied(request.cwd.clone())),
            ScopeVerdict::OutsideRoots => {
                return Err(ToolError::CwdOutOfScope(request.cwd.clone()))
            }
        }

        // RULE 2d: refuse execution-hijacking environment bindings. The whole
        // environment is also surfaced on the approval card (see `proposed_action`)
        // so the approver sees every binding, but these specific names are denied
        // outright — a re-used or auto-granted approval must not be able to smuggle
        // an `LD_PRELOAD`/`RUSTC_WRAPPER`/shadowed-`PATH` past the gate.
        if let Some(binding) = request.environment.iter().find(|b| is_denied_env(&b.name)) {
            return Err(ToolError::EnvironmentNotAllowed(binding.name.clone()));
        }

        // Resolve the program to an absolute path using the daemon's PATH *now*,
        // so the child can run with an emptied environment and still be found.
        let resolved = resolve_program(&request.program, &request.cwd)
            .await
            .ok_or_else(|| ToolError::ProgramNotFound(program_str.clone()))?;

        let mut command = Command::new(&resolved);
        command
            .args(&request.args)
            .current_dir(&request.cwd)
            // RULE 2c: empty environment plus only the explicit bindings.
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for binding in &request.environment {
            command.env(&binding.name, &binding.value);
        }
        // RULE 3: isolate the child (and its descendants) in its own process
        // group so a timeout can terminate the whole group.
        #[cfg(unix)]
        command.process_group(0);

        let started = Instant::now();
        let mut child = command.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ToolError::ProgramNotFound(program_str.clone())
            } else {
                ToolError::Io(e)
            }
        })?;
        let pid = child.id();

        // Drain both pipes concurrently so a chatty child never blocks, capping
        // what is held in memory.
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_task = tokio::spawn(async move { drain(stdout, MAX_CAPTURE_BYTES).await });
        let err_task = tokio::spawn(async move { drain(stderr, MAX_CAPTURE_BYTES).await });

        // RULE 3: enforce the (clamped) timeout, killing the group on expiry.
        let timeout = effective_timeout(request.timeout, command_scope.maximum_seconds);
        let (exit_code, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => (status.code(), false),
            Ok(Err(e)) => return Err(ToolError::Io(e)),
            Err(_elapsed) => {
                kill_group(pid, &mut child).await;
                (None, true)
            }
        };
        let duration = started.elapsed();

        let (stdout_bytes, stdout_overflow) = join_drain(out_task).await?;
        let (stderr_bytes, stderr_overflow) = join_drain(err_task).await?;

        // Spill full output and build the salient view referencing it.
        let stdout_ref = spill(sink, "text/plain", run_id, &stdout_bytes).await?;
        let stderr_ref = spill(sink, "text/plain", run_id, &stderr_bytes).await?;

        let view = salient::SalientView {
            command: display_command(&request.program, &request.args),
            exit_code,
            timed_out,
            duration_ms: duration.as_millis(),
            stdout: salient::compute_stream(&stdout_bytes, stdout_overflow, stdout_ref.clone()),
            stderr: salient::compute_stream(&stderr_bytes, stderr_overflow, stderr_ref.clone()),
        };

        Ok(ShellOutcome {
            exit_code,
            timed_out,
            duration,
            stdout_ref,
            stderr_ref,
            salient: view,
        })
    }
}

/// Whether a model-supplied environment variable name can hijack what the
/// command actually executes: shared-library interposers (`LD_*`/`DYLD_*`),
/// compiler/tool wrappers (`*_WRAPPER`), git's external-program hooks, shell
/// startup files, and `PATH` (which redirects the child's own subprocess
/// lookups even though the top-level program is resolved on the daemon's PATH).
fn is_denied_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "PATH"
            | "GIT_SSH_COMMAND"
            | "GIT_SSH"
            | "GIT_EXTERNAL_DIFF"
            | "GIT_PROXY_COMMAND"
            | "BASH_ENV"
            | "ENV"
            | "SHELLOPTS"
    ) || upper.starts_with("LD_")
        || upper.starts_with("DYLD_")
        || upper.ends_with("_WRAPPER")
}

/// Spill a non-empty stream to the artifact store, returning its reference.
async fn spill(
    sink: &dyn ArtifactSink,
    media_type: &str,
    run_id: RunId,
    bytes: &[u8],
) -> Result<Option<ArtifactRef>, ToolError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let provenance = Provenance::tool_output(Shell::NAME, run_id);
    let reference = sink
        .store(media_type, provenance, bytes)
        .await
        .map_err(ToolError::Other)?;
    Ok(Some(reference))
}

/// Read a taken pipe fully, holding at most `max` bytes in memory and draining
/// (but not retaining) the rest. Returns the captured bytes and whether the cap
/// was exceeded.
async fn drain(
    reader: Option<impl AsyncReadExt + Unpin>,
    max: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let Some(mut reader) = reader else {
        return Ok((Vec::new(), false));
    };
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 8192];
    let mut overflowed = false;
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        if buf.len() < max {
            let take = (max - buf.len()).min(n);
            buf.extend_from_slice(&chunk[..take]);
            if take < n {
                // This chunk pushed us past the cap: keep the prefix, then drain
                // the rest of the pipe to the void so the child never blocks.
                overflowed = true;
                tokio::io::copy(&mut reader, &mut tokio::io::sink()).await?;
                break;
            }
        } else {
            // Already at the cap: drain everything still coming in one shot.
            overflowed = true;
            tokio::io::copy(&mut reader, &mut tokio::io::sink()).await?;
            break;
        }
    }
    Ok((buf, overflowed))
}

/// Await a spawned drain task, flattening the join and I/O errors.
async fn join_drain(
    task: tokio::task::JoinHandle<std::io::Result<(Vec<u8>, bool)>>,
) -> Result<(Vec<u8>, bool), ToolError> {
    match task.await {
        Ok(result) => result.map_err(ToolError::Io),
        Err(join) => Err(ToolError::Other(anyhow::anyhow!(
            "output reader task failed: {join}"
        ))),
    }
}

/// Terminate the child's process group on timeout.
///
/// The child leads its own group (`process_group(0)`). `unsafe` is denied
/// crate-wide and neither `libc` nor `nix` is available, so a true `killpg`
/// syscall is out of reach; we make a best-effort group kill by shelling out to
/// `kill` with a negative pgid, then unconditionally SIGKILL and reap the direct
/// child (which alone covers a single-process command such as `sleep`).
async fn kill_group(pid: Option<u32>, child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(format!("-{pid}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Resolve `program` to an absolute path, searching the daemon PATH for a bare
/// name. Returns `None` when nothing matches. Async so the executable-bit checks
/// never block the runtime thread.
async fn resolve_program(program: &Path, cwd: &Path) -> Option<PathBuf> {
    if program.is_absolute() {
        return is_executable(program).await.then(|| program.to_path_buf());
    }
    if program.components().count() > 1 {
        let joined = cwd.join(program);
        return is_executable(&joined).await.then_some(joined);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if is_executable(&candidate).await {
            return Some(candidate);
        }
    }
    None
}

/// Whether `path` is a regular file with an execute bit set. Checking the bit
/// (not merely existence) skips non-executable shims that shadow a real binary
/// earlier on PATH. Uses async `tokio::fs` so it does not block the runtime.
#[cfg(unix)]
async fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::metadata(path)
        .await
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
async fn is_executable(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false)
}
