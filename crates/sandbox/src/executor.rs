//! Sandbox **enforcement** (STEP 6.2) — the executor that *consumes* a
//! [`SandboxProfile`] and actually confines a process.
//!
//! The [`profile`](crate::profile) module is the compiler that emits a closed
//! profile; this module is the executor that enforces it. A [`SandboxExecutor`]
//! takes a [`SandboxProfile`] plus a [`SandboxCommand`] (program, args, cwd) and
//! runs the command **confined**: a clean environment (only the profile's
//! `env_allowlist` survives), the pre-opened read/write paths and nothing else,
//! the network allowlist (empty ⇒ all network denied), a wall-clock kill that
//! terminates the whole process group, and an output cap. Captured output is
//! [sanitized](crate::sanitize) and origin-labeled before it can ever reach a
//! model transcript — the single untrusted-output chokepoint for sandboxed
//! processes.
//!
//! # Platforms — a real seam with at least one genuine enforcer
//!
//! * **macOS (genuinely enforcing here):** [`seatbelt_profile`] generates a
//!   deterministic Seatbelt (`sandbox-exec`) profile from the [`SandboxProfile`]
//!   — `(deny default)`, `(import "bsd.sb")` for process-startup essentials only,
//!   then the granted read/write paths, the network allowlist, and subprocess
//!   scoping. [`MacosSandbox`] runs the command under `/usr/bin/sandbox-exec`.
//!   The exit-criterion denials (unreadable secret, unreachable host, clean env,
//!   wall-clock kill) are *real* OS denials — see `tests/enforcement_it.rs`.
//! * **Linux:** [`bwrap_argv`] generates a bubblewrap argument vector (bind the
//!   pre-opened paths, `--unshare-net` unless the allowlist is non-empty,
//!   `--clearenv` + per-var `--setenv`, rlimits via a `prlimit` prefix). The
//!   generator is pure and unit-tested on any host; [`LinuxSandbox`] executes it
//!   where `bwrap` is available and fails closed (a typed [`CapabilityReport`],
//!   STEP 6.2.1) where user namespaces / `bwrap` are not.
//! * **Any other platform:** [`enforcing_executor`] returns
//!   [`SandboxError::UnsupportedPlatform`] and [`RefusingSandbox`] refuses every
//!   run — it fails **closed** rather than running a process unconfined.
//!
//! Tool availability is probed at runtime; an absent `sandbox-exec` / `bwrap` is a
//! fail-closed [`SandboxError::ToolUnavailable`] with a legible diagnostic, never a
//! silent downgrade to an unconfined run.
//!
//! # Deliberately out of scope for v1 (deferred, not faked)
//!
//! The `wasmtime` component runtime (STEP 6.3), the brokered-secrets daemon
//! (secrets stay out of `env` here — that invariant *is* enforced — but a full
//! keychain broker is a follow-up), Windows AppContainer, and per-syscall seccomp
//! filtering. Memory/CPU **rlimits** are expressed in the Linux `prlimit` prefix
//! but are *not* enforced on macOS (Seatbelt has no rlimit facility and
//! `setrlimit` needs a `pre_exec` hook, which is `unsafe` — denied workspace-wide);
//! the wall-clock kill and output cap are enforced on every platform. Each gap is
//! surfaced in the [`CapabilityReport`], never hidden.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::profile::SandboxProfile;
use crate::sanitize::{sanitize_untrusted, Sanitized};

/// Which OS mechanism enforces a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxBackend {
    /// macOS Seatbelt via `/usr/bin/sandbox-exec`.
    Seatbelt,
    /// Linux bubblewrap (`bwrap`), optionally wrapped by `prlimit`.
    Bubblewrap,
    /// No enforcing backend on this platform — runs are refused.
    None,
}

impl SandboxBackend {
    /// A stable identifier for logs and the capability report.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxBackend::Seatbelt => "seatbelt",
            SandboxBackend::Bubblewrap => "bubblewrap",
            SandboxBackend::None => "none",
        }
    }
}

/// A typed report of what the active backend actually enforces (STEP 6.2.1).
///
/// This exists so a degraded mode is surfaced **loudly** — a caller renders
/// [`diagnostic`](Self::diagnostic) at install time rather than discovering a
/// silent downgrade at runtime. `degraded` names every property the backend does
/// not fully enforce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityReport {
    /// The OS this report describes (`std::env::consts::OS`).
    pub platform: &'static str,
    /// The enforcing backend.
    pub backend: SandboxBackend,
    /// Whether the backend tool is present and usable right now.
    pub available: bool,
    /// Filesystem confinement to the pre-opened paths.
    pub enforces_filesystem: bool,
    /// Network confinement to the allowlist (empty ⇒ deny-all).
    pub enforces_network: bool,
    /// A clean environment (only `env_allowlist` survives).
    pub enforces_clean_env: bool,
    /// A wall-clock kill terminating the whole process group.
    pub enforces_wall_clock: bool,
    /// The captured-output cap.
    pub enforces_output_cap: bool,
    /// Memory/CPU rlimits.
    pub enforces_rlimits: bool,
    /// Human-legible notes on anything not fully enforced.
    pub degraded: Vec<String>,
}

impl CapabilityReport {
    /// Whether the backend enforces the four exit-criterion properties
    /// (filesystem, network, clean env, wall-clock kill).
    #[must_use]
    pub fn enforces_exit_criteria(&self) -> bool {
        self.available
            && self.enforces_filesystem
            && self.enforces_network
            && self.enforces_clean_env
            && self.enforces_wall_clock
    }

    /// A legible install-time diagnostic: the backend, whether it is usable, and
    /// every degraded property — the "document degraded mode loudly" surface.
    #[must_use]
    pub fn diagnostic(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!(
            "sandbox backend: {} on {} ({})",
            self.backend.as_str(),
            self.platform,
            if self.available {
                "available"
            } else {
                "UNAVAILABLE — runs fail closed"
            },
        );
        if self.degraded.is_empty() {
            out.push_str("; no degraded properties");
        } else {
            out.push_str("; degraded: ");
            for (i, note) in self.degraded.iter().enumerate() {
                if i > 0 {
                    let _ = write!(out, "; ");
                }
                let _ = write!(out, "{note}");
            }
        }
        out
    }
}

/// A command to run confined: a structured request, never an unparsed shell
/// string.
#[derive(Debug, Clone)]
pub struct SandboxCommand {
    /// The program to run — an **absolute** path (the sandbox denies a PATH search
    /// its own filesystem rules would block anyway).
    pub program: std::path::PathBuf,
    /// Arguments, passed verbatim.
    pub args: Vec<String>,
    /// Working directory. Should be within a granted path.
    pub cwd: std::path::PathBuf,
    /// Origin label the captured output is tagged with (e.g. `skill:rust.fix-ci`,
    /// `plugin:github`) so sanitized output enters context as labeled evidence.
    pub origin: String,
}

impl SandboxCommand {
    /// Construct a command.
    pub fn new(
        program: impl Into<std::path::PathBuf>,
        args: Vec<String>,
        cwd: impl Into<std::path::PathBuf>,
        origin: impl Into<String>,
    ) -> Self {
        Self {
            program: program.into(),
            args,
            cwd: cwd.into(),
            origin: origin.into(),
        }
    }
}

/// The captured, sanitized result of a confined run — the structured audit
/// record. `exit_code` is `None` when the process was killed (wall-clock breach).
#[derive(Debug, Clone)]
pub struct SandboxOutcome {
    /// The backend that ran the command.
    pub backend: SandboxBackend,
    /// Process exit code, or `None` if the process was killed.
    pub exit_code: Option<i32>,
    /// Whether the wall-clock cap was hit and the process group killed.
    pub timed_out: bool,
    /// Wall-clock duration of the run.
    pub duration: Duration,
    /// Sanitized, origin-labeled standard output (control-stripped, size-capped).
    pub stdout: Sanitized,
    /// Sanitized, origin-labeled standard error.
    pub stderr: Sanitized,
    /// Whether captured output exceeded the cap (spilled/truncated).
    pub output_truncated: bool,
}

impl SandboxOutcome {
    /// Whether the command completed successfully (zero exit, not killed).
    #[must_use]
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out
    }

    /// Whether the run was denied or killed — the shape a filesystem/network
    /// denial or a wall-clock breach takes (non-zero exit or a kill).
    #[must_use]
    pub fn denied(&self) -> bool {
        !self.success()
    }

    /// A one-line audit summary of the run (safe to log — output is not included).
    #[must_use]
    pub fn audit_summary(&self) -> String {
        format!(
            "sandbox[{}] origin={} exit={} timed_out={} duration_ms={} out_bytes={} err_bytes={} stripped={} truncated={}",
            self.backend.as_str(),
            self.stdout.origin,
            self.exit_code
                .map_or_else(|| "killed".to_string(), |c| c.to_string()),
            self.timed_out,
            self.duration.as_millis(),
            self.stdout.text.len(),
            self.stderr.text.len(),
            self.stdout.stripped_controls + self.stderr.stripped_controls,
            self.output_truncated,
        )
    }
}

/// Why a confined run could not be performed. Every variant is a **fail-closed**
/// refusal: the executor never falls back to running a process unconfined.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// No OS sandbox backend exists for this platform.
    #[error("no OS sandbox backend on this platform ({platform}); refusing to run unconfined")]
    UnsupportedPlatform {
        /// The platform with no backend.
        platform: &'static str,
    },
    /// The backend tool (`sandbox-exec` / `bwrap`) is not available.
    #[error("sandbox tool `{tool}` is unavailable: {diagnostic}; refusing to run unconfined")]
    ToolUnavailable {
        /// The tool that is missing.
        tool: String,
        /// A legible diagnostic for the operator.
        diagnostic: String,
    },
    /// The command was structurally invalid (e.g. a non-absolute program path).
    #[error("invalid sandbox command: {0}")]
    InvalidCommand(String),
    /// Spawning the sandboxed process failed.
    #[error("spawning the sandboxed process failed: {0}")]
    Spawn(#[source] std::io::Error),
    /// An I/O error while running or reaping the process.
    #[error("sandbox I/O error: {0}")]
    Io(#[source] std::io::Error),
}

/// The enforcement seam: consume a [`SandboxProfile`] and run a command confined.
pub trait SandboxExecutor {
    /// What this executor actually enforces on this host (STEP 6.2.1).
    fn capability_report(&self) -> CapabilityReport;

    /// Run `command` confined by `profile`, returning the sanitized outcome.
    /// Fails **closed** on any setup problem — it never runs the command
    /// unconfined.
    fn run(
        &self,
        profile: &SandboxProfile,
        command: &SandboxCommand,
    ) -> Result<SandboxOutcome, SandboxError>;
}

/// Resolve the enforcing executor for the current platform, or fail closed.
///
/// Returns a real, tool-probed backend on macOS/Linux; on any other platform it
/// returns [`SandboxError::UnsupportedPlatform`] rather than an unconfined runner.
pub fn enforcing_executor() -> Result<Box<dyn SandboxExecutor>, SandboxError> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(MacosSandbox::new()?))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(LinuxSandbox::new()?))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(SandboxError::UnsupportedPlatform {
            platform: std::env::consts::OS,
        })
    }
}

/// An executor that refuses every run — the fail-closed posture for a platform
/// with no OS sandbox backend. Constructible everywhere so the refuse-path is
/// deterministically testable on any host.
#[derive(Debug, Default, Clone, Copy)]
pub struct RefusingSandbox;

impl SandboxExecutor for RefusingSandbox {
    fn capability_report(&self) -> CapabilityReport {
        CapabilityReport {
            platform: std::env::consts::OS,
            backend: SandboxBackend::None,
            available: false,
            enforces_filesystem: false,
            enforces_network: false,
            enforces_clean_env: false,
            enforces_wall_clock: false,
            enforces_output_cap: false,
            enforces_rlimits: false,
            degraded: vec![
                "no OS sandbox backend on this platform; every run is refused (fail closed)".into(),
            ],
        }
    }

    fn run(
        &self,
        _profile: &SandboxProfile,
        _command: &SandboxCommand,
    ) -> Result<SandboxOutcome, SandboxError> {
        Err(SandboxError::UnsupportedPlatform {
            platform: std::env::consts::OS,
        })
    }
}

// ---------------------------------------------------------------------------
// macOS Seatbelt profile generation (pure — compiled and unit-tested on any host)
// ---------------------------------------------------------------------------

/// Generate the deterministic Seatbelt profile text (SBPL) for `profile`, running
/// `target_program`.
///
/// The profile is **closed**: `(deny default)` denies everything, `(import
/// "bsd.sb")` re-allows only the OS primitives a process needs to *start* (the
/// dynamic loader, system frameworks, mach/sysctl basics — no user-data
/// *contents*; note that Apple's base profile does leave file **metadata**
/// (`stat`: existence/size/mtime/mode) enumerable anywhere, surfaced in the
/// [`CapabilityReport`]), and then exactly the profile's grants are layered on:
///
/// * each `read_paths` entry → `(allow file-read* (subpath …))`;
/// * each `write_paths` entry → `(allow file-read* file-write* (subpath …))`;
/// * a non-empty `network_allowlist` → `(allow network-outbound (remote ip))`
///   (empty ⇒ the default denial stands — all network denied);
/// * `process-exec*` is scoped to just `target_program` unless `allow_subprocess`,
///   so a plugin that declared no subprocess capability cannot exec a child.
///
/// Anything not named is denied by `(deny default)`, so a secret outside
/// `read_paths` or a host outside the allowlist fails structurally. Generation is
/// pure and deterministic (identical fields ⇒ identical text), so it is testable
/// without executing anything.
#[must_use]
pub fn seatbelt_profile(profile: &SandboxProfile, target_program: &Path) -> String {
    let mut sb = String::new();
    sb.push_str("(version 1)\n");
    // Closed by default: files, network, mach, everything not re-allowed below.
    sb.push_str("(deny default)\n");
    // Re-allow ONLY process-startup essentials (loader, system frameworks, mach,
    // sysctl). bsd.sb grants no access to user-data *contents* — a planted secret
    // outside the granted read paths stays unreadable (verified by the enforcement
    // test). It DOES leave file metadata (`stat`) enumerable anywhere; that surface
    // is named in the capability report rather than silently accepted.
    sb.push_str("(import \"bsd.sb\")\n");
    sb.push_str("(allow process-fork)\n");
    if profile.allow_subprocess {
        sb.push_str("(allow process-exec*)\n");
    } else {
        // Scope exec to just the target so it can start but cannot spawn other
        // programs — the OS half of `allow_subprocess = false`.
        let _ = std::fmt::Write::write_fmt(
            &mut sb,
            format_args!(
                "(allow process-exec* (literal \"{}\"))\n",
                sbpl_escape(&target_program.to_string_lossy())
            ),
        );
    }

    for path in &profile.read_paths {
        if let Some(abs) = sbpl_subpath(path) {
            let _ = std::fmt::Write::write_fmt(
                &mut sb,
                format_args!("(allow file-read* (subpath \"{abs}\"))\n"),
            );
        }
    }
    for path in &profile.write_paths {
        if let Some(abs) = sbpl_subpath(path) {
            let _ = std::fmt::Write::write_fmt(
                &mut sb,
                format_args!("(allow file-read* file-write* (subpath \"{abs}\"))\n"),
            );
        }
    }
    if !profile.network_allowlist.is_empty() {
        // SBPL cannot filter outbound by resolved hostname; v1 coarsens a non-empty
        // allowlist to "outbound IP permitted" (host:port granularity needs a
        // brokering proxy — deferred). The headline denial — an EMPTY allowlist ⇒
        // all network denied — is exact.
        sb.push_str("(allow network-outbound (remote ip))\n");
    }
    sb
}

/// Escape a path for use inside an SBPL `(subpath "…")`: backslash and double
/// quote. Returns `None` for an empty or non-absolute path (Seatbelt subpaths must
/// be absolute).
fn sbpl_subpath(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        return None;
    }
    Some(sbpl_escape(trimmed))
}

/// Escape backslash and double-quote for an SBPL string literal.
fn sbpl_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---------------------------------------------------------------------------
// Linux bubblewrap argument generation (pure — compiled and unit-tested on any host)
// ---------------------------------------------------------------------------

/// Generate the complete argument vector for a bubblewrap-confined run.
///
/// The vector is the whole command line to spawn: an optional `prlimit` prefix
/// (when the profile sets memory/CPU caps — bwrap has no rlimit facility), then
/// `bwrap` with:
///
/// * `--die-with-parent`, `--new-session`, `--unshare-user`/`--unshare-ipc`/
///   `--unshare-pid`/`--unshare-uts`/`--unshare-cgroup` — a fresh namespace set;
/// * `--unshare-net` **unless** `network_allowlist` is non-empty (so an empty
///   allowlist ⇒ no network at all);
/// * `--clearenv` then a `--setenv NAME VALUE` for each `env_allowlist` var that is
///   set in the parent — the clean-environment guarantee;
/// * `--proc /proc`, `--dev /dev`, `--tmpfs /tmp`, and read-only binds of the
///   system directories a program needs to run;
/// * `--ro-bind` each `read_paths` entry and `--bind` each `write_paths` entry;
/// * `--chdir <cwd>`, then `-- <program> <args…>`.
///
/// This is pure and deterministic so it is unit-tested on any host even where
/// `bwrap` cannot be executed; [`LinuxSandbox`] spawns the result.
#[must_use]
pub fn bwrap_argv(profile: &SandboxProfile, command: &SandboxCommand) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();

    // rlimits (bwrap has none): a `prlimit` prefix applies memory/CPU caps.
    if profile.memory_mb > 0 || profile.cpu_seconds > 0 {
        argv.push("prlimit".into());
        if profile.cpu_seconds > 0 {
            argv.push(format!("--cpu={}", profile.cpu_seconds));
        }
        if profile.memory_mb > 0 {
            let bytes = profile.memory_mb.saturating_mul(1024 * 1024);
            argv.push(format!("--as={bytes}"));
        }
        argv.push("--".into());
    }

    argv.push("bwrap".into());
    argv.push("--die-with-parent".into());
    argv.push("--new-session".into());
    for ns in [
        "--unshare-user",
        "--unshare-ipc",
        "--unshare-pid",
        "--unshare-uts",
        "--unshare-cgroup",
    ] {
        argv.push(ns.into());
    }
    // Network: unshare (deny) unless the allowlist is non-empty. bwrap alone cannot
    // filter by host; a non-empty allowlist shares the host network namespace and
    // relies on the profile/broker for host granularity (deferred — documented).
    if profile.network_allowlist.is_empty() {
        argv.push("--unshare-net".into());
    }

    // Clean environment: clear, then re-add only the allowlisted vars that exist.
    argv.push("--clearenv".into());
    for name in &profile.env_allowlist {
        if let Some(value) = std::env::var_os(name) {
            argv.push("--setenv".into());
            argv.push(name.clone());
            argv.push(value.to_string_lossy().into_owned());
        }
    }

    // Base filesystem: a private /proc, /dev, and /tmp, plus read-only system dirs
    // the program needs to run. Nothing under the user's home is bound.
    argv.push("--proc".into());
    argv.push("/proc".into());
    argv.push("--dev".into());
    argv.push("/dev".into());
    argv.push("--tmpfs".into());
    argv.push("/tmp".into());
    for sysdir in ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"] {
        argv.push("--ro-bind-try".into());
        argv.push(sysdir.into());
        argv.push(sysdir.into());
    }

    // Granted paths.
    for path in &profile.read_paths {
        argv.push("--ro-bind".into());
        argv.push(path.clone());
        argv.push(path.clone());
    }
    for path in &profile.write_paths {
        argv.push("--bind".into());
        argv.push(path.clone());
        argv.push(path.clone());
    }

    argv.push("--chdir".into());
    argv.push(command.cwd.to_string_lossy().into_owned());
    argv.push("--".into());
    argv.push(command.program.to_string_lossy().into_owned());
    for a in &command.args {
        argv.push(a.clone());
    }
    argv
}

// ---------------------------------------------------------------------------
// Shared execution harness (std::process — no async runtime in this crate)
// ---------------------------------------------------------------------------

/// Poll interval while waiting for the child to exit or the wall-clock to expire.
const WAIT_POLL: Duration = Duration::from_millis(10);
/// Chunk size for reading captured output.
const READ_CHUNK: usize = 8 * 1024;

/// Spawn `argv`, capture (capped) output, enforce the wall-clock kill on the whole
/// process group, and return the sanitized outcome. The environment is emptied and
/// only `env_allowlist` vars present in the parent are re-added.
fn spawn_capture_kill(
    argv: &[String],
    cwd: &Path,
    env_allowlist: &[String],
    wall: Duration,
    output_cap_bytes: usize,
    origin: &str,
    backend: SandboxBackend,
) -> Result<SandboxOutcome, SandboxError> {
    let Some((program, rest)) = argv.split_first() else {
        return Err(SandboxError::InvalidCommand("empty command".into()));
    };

    let mut cmd = Command::new(program);
    cmd.args(rest)
        .current_dir(cwd)
        // Clean environment: nothing inherited except the allowlisted vars.
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in env_allowlist {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    // Lead a fresh process group so a wall-clock kill terminates the whole group,
    // not just the direct child. Safe API (no `unsafe`), stable since Rust 1.64.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }

    let started = Instant::now();
    let mut child = cmd.spawn().map_err(SandboxError::Spawn)?;
    let pid = child.id();

    // Drain both pipes on their own threads so a chatty child never deadlocks on a
    // full pipe buffer, capping what is held in memory.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_handle = thread::spawn(move || read_capped(stdout, output_cap_bytes));
    let err_handle = thread::spawn(move || read_capped(stderr, output_cap_bytes));

    // Wait for exit, or kill the group on wall-clock expiry. A zero wall means no
    // wall-clock cap (the caller opted out); every profile derived from a manifest
    // carries a non-zero `wall_seconds`.
    let mut timed_out = false;
    let mut exit_code = None;
    let deadline = if wall.is_zero() {
        None
    } else {
        Some(started + wall)
    };
    loop {
        match child.try_wait().map_err(SandboxError::Io)? {
            Some(status) => {
                exit_code = status.code();
                break;
            }
            None => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    #[cfg(unix)]
                    kill_process_group(pid);
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                thread::sleep(WAIT_POLL);
            }
        }
    }
    let duration = started.elapsed();

    // Killing the group closes the write ends, so the reader threads reach EOF.
    let (out_bytes, out_over) = out_handle.join().unwrap_or_else(|_| (Vec::new(), true));
    let (err_bytes, err_over) = err_handle.join().unwrap_or_else(|_| (Vec::new(), true));

    // THE untrusted-output chokepoint: everything a sandboxed process emits is
    // sanitized (control-stripped, size-capped) and origin-labeled before it can
    // reach a transcript — one boundary function, not scattered call sites.
    let stdout = sanitize_untrusted(
        origin,
        &String::from_utf8_lossy(&out_bytes),
        output_cap_bytes,
    );
    let stderr = sanitize_untrusted(
        format!("{origin} (stderr)"),
        &String::from_utf8_lossy(&err_bytes),
        output_cap_bytes,
    );
    let output_truncated = out_over || err_over || stdout.truncated || stderr.truncated;

    Ok(SandboxOutcome {
        backend,
        exit_code,
        timed_out,
        duration,
        stdout,
        stderr,
        output_truncated,
    })
}

/// Read a pipe fully, retaining at most `cap` bytes and draining the rest to the
/// void (so the child never blocks on a full buffer). Returns the captured bytes
/// and whether the cap was exceeded.
fn read_capped(reader: Option<impl Read>, cap: usize) -> (Vec<u8>, bool) {
    let Some(mut reader) = reader else {
        return (Vec::new(), false);
    };
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut overflowed = false;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = (cap - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                    if take < n {
                        overflowed = true;
                        drain_to_void(&mut reader);
                        break;
                    }
                } else {
                    overflowed = true;
                    drain_to_void(&mut reader);
                    break;
                }
            }
            Err(_) => break,
        }
    }
    (buf, overflowed)
}

/// Drain the rest of a reader without retaining it.
fn drain_to_void(reader: &mut impl Read) {
    let mut sink = vec![0u8; READ_CHUNK];
    while let Ok(n) = reader.read(&mut sink) {
        if n == 0 {
            break;
        }
    }
}

/// Terminate the child's process group on a wall-clock breach.
///
/// `unsafe` is denied workspace-wide and neither `libc` nor `nix` is a dependency,
/// so a direct `killpg(2)` is out of reach; we make a best-effort group kill by
/// shelling out to `kill` with a negative pgid (the child leads its own group via
/// `process_group(0)`). The caller also SIGKILLs and reaps the direct child.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(format!("-{pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Whether `path` is a regular file with an execute bit — the tool-availability
/// probe. Checking the bit (not mere existence) is the fail-closed posture.
fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(path)
            .map(|m| m.is_file())
            .unwrap_or(false)
    }
}

/// Locate `tool` on `PATH`, returning the first executable match. Used by the
/// Linux availability probe (e.g. finding `bwrap`/`prlimit`); the macOS backend
/// resolves `sandbox-exec` at a fixed path, so this is Linux/test-only.
#[cfg(any(target_os = "linux", test))]
fn locate_on_path(tool: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|candidate| is_executable_file(candidate))
}

// ---------------------------------------------------------------------------
// macOS backend
// ---------------------------------------------------------------------------

/// The macOS Seatbelt executor. Runs commands under `/usr/bin/sandbox-exec` with a
/// [`seatbelt_profile`] generated from the [`SandboxProfile`].
#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
pub struct MacosSandbox {
    tool: std::path::PathBuf,
}

#[cfg(target_os = "macos")]
impl MacosSandbox {
    /// The Seatbelt front-end.
    const TOOL: &'static str = "/usr/bin/sandbox-exec";

    /// Probe for `sandbox-exec` and construct the executor, or fail closed.
    pub fn new() -> Result<Self, SandboxError> {
        let tool = std::path::PathBuf::from(Self::TOOL);
        if !is_executable_file(&tool) {
            return Err(SandboxError::ToolUnavailable {
                tool: Self::TOOL.into(),
                diagnostic: format!("{} is not an executable file", Self::TOOL),
            });
        }
        Ok(Self { tool })
    }
}

#[cfg(target_os = "macos")]
impl SandboxExecutor for MacosSandbox {
    fn capability_report(&self) -> CapabilityReport {
        CapabilityReport {
            platform: std::env::consts::OS,
            backend: SandboxBackend::Seatbelt,
            available: is_executable_file(&self.tool),
            enforces_filesystem: true,
            enforces_network: true,
            enforces_clean_env: true,
            enforces_wall_clock: true,
            enforces_output_cap: true,
            // Seatbelt has no rlimit facility and setrlimit needs an `unsafe`
            // pre_exec hook (denied workspace-wide); memory/CPU caps are not
            // enforced here (wall-clock + output ARE).
            enforces_rlimits: false,
            degraded: vec![
                "memory/CPU rlimits not enforced by Seatbelt (wall-clock kill + output cap are)"
                    .into(),
                "network allowlist coarsened to outbound-IP (host:port granularity deferred to a broker)"
                    .into(),
                "file METADATA (stat: existence/size/mtime/mode) is enumerable anywhere via Apple's \
                 base profile (bsd.sb); file CONTENTS outside the granted read paths stay denied"
                    .into(),
                "process-group kill is best-effort: a subprocess that calls setsid()/setpgid() (only \
                 reachable when allow_subprocess) can escape the group kill, though the direct child \
                 is still killed"
                    .into(),
            ],
        }
    }

    fn run(
        &self,
        profile: &SandboxProfile,
        command: &SandboxCommand,
    ) -> Result<SandboxOutcome, SandboxError> {
        if !command.program.is_absolute() {
            return Err(SandboxError::InvalidCommand(format!(
                "program must be an absolute path, got `{}`",
                command.program.display()
            )));
        }
        // Seatbelt matches paths *after* symlink resolution, so the granted
        // subpaths must be canonical or the kernel-resolved path (e.g. macOS
        // `/var/folders/…` → `/private/var/folders/…`) never matches and the grant
        // is silently ineffective. Resolve the grants, cwd, and program to their
        // canonical forms so the profile means what it says.
        let program = canonicalize_grant(&command.program.to_string_lossy());
        let cwd = std::path::PathBuf::from(canonicalize_grant(&command.cwd.to_string_lossy()));
        let mut resolved = profile.clone();
        resolved.read_paths = profile
            .read_paths
            .iter()
            .map(|p| canonicalize_grant(p))
            .collect();
        resolved.write_paths = profile
            .write_paths
            .iter()
            .map(|p| canonicalize_grant(p))
            .collect();

        let sb = seatbelt_profile(&resolved, Path::new(&program));
        let mut argv = vec![
            self.tool.to_string_lossy().into_owned(),
            "-p".into(),
            sb,
            program,
        ];
        argv.extend(command.args.iter().cloned());

        spawn_capture_kill(
            &argv,
            &cwd,
            &profile.env_allowlist,
            Duration::from_secs(profile.wall_seconds),
            output_cap_bytes(profile),
            &command.origin,
            SandboxBackend::Seatbelt,
        )
    }
}

/// Resolve `path` to its canonical (symlink-free) form so a Seatbelt `(subpath …)`
/// rule matches the kernel's resolved path. For a path that does not exist yet (a
/// write target), canonicalize the nearest existing ancestor and re-append the
/// remainder; if nothing resolves, keep the original.
#[cfg(target_os = "macos")]
fn canonicalize_grant(path: &str) -> String {
    let p = Path::new(path);
    if let Ok(canon) = std::fs::canonicalize(p) {
        return canon.to_string_lossy().into_owned();
    }
    let mut ancestors = p.ancestors();
    let _self = ancestors.next();
    for ancestor in ancestors {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        if let Ok(canon) = std::fs::canonicalize(ancestor) {
            if let Ok(rest) = p.strip_prefix(ancestor) {
                return canon.join(rest).to_string_lossy().into_owned();
            }
        }
    }
    path.to_string()
}

// ---------------------------------------------------------------------------
// Linux backend
// ---------------------------------------------------------------------------

/// The Linux bubblewrap executor. Runs commands under `bwrap` with an argument
/// vector generated by [`bwrap_argv`].
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub struct LinuxSandbox {
    tool: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
impl LinuxSandbox {
    /// Probe for `bwrap` on `PATH` and construct the executor, or fail closed.
    pub fn new() -> Result<Self, SandboxError> {
        let tool = locate_on_path("bwrap").ok_or_else(|| SandboxError::ToolUnavailable {
            tool: "bwrap".into(),
            diagnostic: "bubblewrap (`bwrap`) not found on PATH; install bubblewrap or run on a \
                         host with user namespaces"
                .into(),
        })?;
        Ok(Self { tool })
    }
}

#[cfg(target_os = "linux")]
impl SandboxExecutor for LinuxSandbox {
    fn capability_report(&self) -> CapabilityReport {
        let available = is_executable_file(&self.tool);
        let mut degraded = vec![
            "network allowlist coarsened: a non-empty allowlist shares the host network namespace \
             (host:port granularity deferred to a broker)"
                .into(),
            "process-group kill is best-effort: a subprocess that calls setsid()/setpgid() (only \
             reachable when allow_subprocess) can escape the group kill, though the direct child \
             is still killed"
                .into(),
        ];
        if locate_on_path("prlimit").is_none() {
            degraded
                .push("prlimit not found on PATH; memory/CPU rlimits will not be applied".into());
        }
        CapabilityReport {
            platform: std::env::consts::OS,
            backend: SandboxBackend::Bubblewrap,
            available,
            enforces_filesystem: true,
            enforces_network: true,
            enforces_clean_env: true,
            enforces_wall_clock: true,
            enforces_output_cap: true,
            enforces_rlimits: locate_on_path("prlimit").is_some(),
            degraded,
        }
    }

    fn run(
        &self,
        profile: &SandboxProfile,
        command: &SandboxCommand,
    ) -> Result<SandboxOutcome, SandboxError> {
        if !command.program.is_absolute() {
            return Err(SandboxError::InvalidCommand(format!(
                "program must be an absolute path, got `{}`",
                command.program.display()
            )));
        }
        // `bwrap_argv` names `bwrap`/`prlimit` bare; resolve `bwrap` to the probed
        // absolute path so the spawn does not depend on PATH ordering.
        let mut argv = bwrap_argv(profile, command);
        if let Some(first) = argv.iter_mut().find(|a| a.as_str() == "bwrap") {
            *first = self.tool.to_string_lossy().into_owned();
        }
        spawn_capture_kill(
            &argv,
            &command.cwd,
            &profile.env_allowlist,
            Duration::from_secs(profile.wall_seconds),
            output_cap_bytes(profile),
            &command.origin,
            SandboxBackend::Bubblewrap,
        )
    }
}

/// The captured-output byte cap for a profile (`maximum_output_mb` → bytes, with a
/// small floor so a zero cap still admits a line of diagnostic output).
fn output_cap_bytes(profile: &SandboxProfile) -> usize {
    let bytes = (profile.maximum_output_mb as usize).saturating_mul(1024 * 1024);
    bytes.max(4 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{parse_manifest, CapabilitiesSpec};
    use crate::permission::CapabilitySet;

    fn profile_from(toml: &str) -> SandboxProfile {
        let m = parse_manifest(toml).unwrap();
        let granted = CapabilitySet::from_spec(&m.capabilities);
        SandboxProfile::derive(&m, &granted)
    }

    fn sample_profile() -> SandboxProfile {
        profile_from(
            r#"
schema_version = 1
id = "github"
name = "GitHub"
version = "0.1.0"
kind = "native-process"
publisher = "codypendent-project"
[runtime]
command = "codypendent-plugin-github"
[capabilities]
filesystem_read = ["/workspace/repo"]
filesystem_write = ["/workspace/repo/target"]
network = ["api.github.com:443"]
subprocess = false
[resources]
memory_mb = 256
cpu_seconds = 60
wall_seconds = 120
maximum_output_mb = 20
"#,
        )
    }

    // --- Seatbelt profile generation (pure; runs on every platform) ---

    #[test]
    fn seatbelt_profile_is_closed_and_lists_grants() {
        let p = sample_profile();
        let sb = seatbelt_profile(&p, Path::new("/bin/cat"));
        assert!(sb.starts_with("(version 1)\n(deny default)\n(import \"bsd.sb\")\n"));
        assert!(sb.contains("(allow file-read* (subpath \"/workspace/repo\"))"));
        assert!(sb.contains("(allow file-read* file-write* (subpath \"/workspace/repo/target\"))"));
        assert!(sb.contains("(allow network-outbound (remote ip))"));
        // subprocess = false ⇒ exec scoped to just the target program.
        assert!(sb.contains("(allow process-exec* (literal \"/bin/cat\"))"));
        assert!(!sb.contains("(allow process-exec*)\n"));
    }

    #[test]
    fn seatbelt_profile_is_deterministic() {
        let p = sample_profile();
        assert_eq!(
            seatbelt_profile(&p, Path::new("/bin/cat")),
            seatbelt_profile(&p, Path::new("/bin/cat")),
        );
    }

    #[test]
    fn seatbelt_empty_network_denies_all_network() {
        let p = profile_from(
            r#"
schema_version = 1
id = "x"
name = "X"
version = "0.1.0"
kind = "native-process"
publisher = "me"
[runtime]
command = "x"
[capabilities]
subprocess = true
"#,
        );
        let sb = seatbelt_profile(&p, Path::new("/bin/echo"));
        // No network grant ⇒ (deny default) leaves all network denied.
        assert!(!sb.contains("network-outbound"));
        // subprocess = true ⇒ broad exec.
        assert!(sb.contains("(allow process-exec*)\n"));
    }

    #[test]
    fn seatbelt_escapes_quotes_in_paths() {
        let mut p = sample_profile();
        p.read_paths = vec!["/tmp/we\"ird".into()];
        let sb = seatbelt_profile(&p, Path::new("/bin/cat"));
        assert!(sb.contains(r#"(subpath "/tmp/we\"ird")"#));
    }

    #[test]
    fn seatbelt_skips_non_absolute_read_paths() {
        let mut p = sample_profile();
        p.read_paths = vec!["relative/path".into(), "/abs/ok".into()];
        let sb = seatbelt_profile(&p, Path::new("/bin/cat"));
        assert!(!sb.contains("relative/path"));
        assert!(sb.contains("(subpath \"/abs/ok\")"));
    }

    // --- bubblewrap argument generation (pure; runs on every platform) ---

    #[test]
    fn bwrap_argv_denies_network_and_clears_env_when_no_allowlist() {
        let p = profile_from(
            r#"
schema_version = 1
id = "x"
name = "X"
version = "0.1.0"
kind = "native-process"
publisher = "me"
[runtime]
command = "x"
[capabilities]
filesystem_read = ["/work"]
[resources]
memory_mb = 0
cpu_seconds = 0
wall_seconds = 30
maximum_output_mb = 8
"#,
        );
        let cmd = SandboxCommand::new("/bin/cat", vec!["/work/f".into()], "/work", "plugin:x");
        let argv = bwrap_argv(&p, &cmd);
        assert_eq!(argv.first().map(String::as_str), Some("bwrap"));
        assert!(argv.iter().any(|a| a == "--unshare-net"));
        assert!(argv.iter().any(|a| a == "--clearenv"));
        assert!(argv
            .windows(3)
            .any(|w| w == ["--ro-bind", "/work", "/work"]));
        // program + args come after the `--` separator, in order.
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(argv[sep + 1], "/bin/cat");
        assert_eq!(argv[sep + 2], "/work/f");
    }

    #[test]
    fn bwrap_argv_shares_net_and_prefixes_prlimit_with_caps() {
        let p = sample_profile(); // network non-empty, memory 256, cpu 60
        let cmd = SandboxCommand::new("/bin/echo", vec![], "/workspace/repo", "plugin:github");
        let argv = bwrap_argv(&p, &cmd);
        // rlimit caps ⇒ prlimit prefix before bwrap.
        assert_eq!(argv.first().map(String::as_str), Some("prlimit"));
        assert!(argv.iter().any(|a| a == "--cpu=60"));
        assert!(argv
            .iter()
            .any(|a| a == &format!("--as={}", 256u64 * 1024 * 1024)));
        // Non-empty network allowlist ⇒ net namespace NOT unshared.
        assert!(!argv.iter().any(|a| a == "--unshare-net"));
        // write path is a read-write bind.
        assert!(argv
            .windows(3)
            .any(|w| w == ["--bind", "/workspace/repo/target", "/workspace/repo/target"]));
    }

    // --- fail-closed posture (runs on every platform) ---

    #[test]
    fn refusing_sandbox_refuses_every_run() {
        let p = sample_profile();
        let cmd = SandboxCommand::new("/bin/echo", vec![], "/", "plugin:x");
        let err = RefusingSandbox.run(&p, &cmd).unwrap_err();
        assert!(matches!(err, SandboxError::UnsupportedPlatform { .. }));
        let report = RefusingSandbox.capability_report();
        assert!(!report.available);
        assert!(!report.enforces_exit_criteria());
        assert_eq!(report.backend, SandboxBackend::None);
    }

    #[test]
    fn availability_probe_fails_closed_on_a_missing_tool() {
        // The probe the Linux/macOS constructors use: a non-existent tool path is
        // not executable, so construction would fail closed rather than run
        // unconfined.
        assert!(!is_executable_file(Path::new("/nonexistent/bwrap")));
        assert!(locate_on_path("codypendent-definitely-not-a-real-tool-xyz").is_none());
    }

    #[test]
    fn capability_report_diagnostic_is_loud_about_degradation() {
        let report = RefusingSandbox.capability_report();
        let diag = report.diagnostic();
        assert!(diag.contains("UNAVAILABLE"));
        assert!(diag.contains("fail closed"));
    }

    #[test]
    fn output_cap_has_a_floor() {
        let mut p = sample_profile();
        p.maximum_output_mb = 0;
        assert_eq!(output_cap_bytes(&p), 4 * 1024);
        p.maximum_output_mb = 2;
        assert_eq!(output_cap_bytes(&p), 2 * 1024 * 1024);
    }

    #[test]
    fn sandbox_command_is_structured() {
        let cmd = SandboxCommand::new("/bin/echo", vec!["hi".into()], "/tmp", "plugin:x");
        assert_eq!(cmd.program, Path::new("/bin/echo"));
        assert_eq!(cmd.args, ["hi"]);
        assert_eq!(cmd.origin, "plugin:x");
        // A profile with no grants at all still derives (deny-everything).
        let empty = CapabilitySet::from_spec(&CapabilitiesSpec::default());
        let m = parse_manifest(
            r#"
schema_version = 1
id = "x"
name = "X"
version = "0.1.0"
kind = "native-process"
publisher = "me"
[runtime]
command = "x"
"#,
        )
        .unwrap();
        let p = SandboxProfile::derive(&m, &empty);
        assert!(p.read_paths.is_empty() && p.network_allowlist.is_empty());
    }
}
