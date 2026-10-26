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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

/// Maximum bytes written to a sandboxed command's stdin (1 MiB — adoption 08).
pub const MAX_STDIN_BYTES: usize = 1024 * 1024;

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
    /// Bytes written to the child's stdin before it is closed, or `None` for
    /// the existing null-stdin behaviour (adoption 08 — the hook payload
    /// protocol). Bounded: larger than [`MAX_STDIN_BYTES`] is refused as
    /// [`SandboxError::InvalidCommand`], never truncated.
    pub stdin: Option<Vec<u8>>,
    /// True only when a trusted host runtime independently denies process
    /// creation (for example Node's stable `--permission` model without
    /// `--allow-child-process`). This lets Linux admit that specific runtime
    /// while continuing to reject generic no-subprocess commands whose bound
    /// system binaries would otherwise remain executable inside bubblewrap.
    runtime_denies_subprocess: bool,
}

/// A fully lowered command line for a long-lived, stdio-connected sandboxed
/// process. The argv always starts with the enforcing backend
/// (`sandbox-exec`/`bwrap`, optionally `prlimit`), never the untrusted program
/// directly. Consumers use this for protocols that cannot be expressed as the
/// capture-to-completion [`SandboxExecutor::run`] call.
///
/// The fields are private so callers cannot accidentally widen the profile
/// after policy has been lowered. In particular, the clean environment and
/// sandbox working directory are frozen at preparation time.
#[derive(Debug, Clone)]
pub struct SandboxProcessSpec {
    backend: SandboxBackend,
    argv: Vec<String>,
    cwd: std::path::PathBuf,
    environment: Vec<(String, std::ffi::OsString)>,
    origin: String,
    wall_clock: Duration,
    output_cap_bytes: usize,
}

impl SandboxProcessSpec {
    /// The OS boundary wrapped around the process.
    #[must_use]
    pub fn backend(&self) -> SandboxBackend {
        self.backend
    }

    /// Complete structured argv, including the sandbox frontend as argv[0].
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// Isolated working directory selected by the host.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Frozen clean environment. No parent variable outside this sequence may
    /// be inherited when the process is spawned.
    #[must_use]
    pub fn environment(&self) -> &[(String, std::ffi::OsString)] {
        &self.environment
    }

    /// Untrusted-origin label for diagnostics and audit records.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Maximum lifetime granted by the sandbox profile.
    #[must_use]
    pub fn wall_clock(&self) -> Duration {
        self.wall_clock
    }

    /// Maximum retained diagnostic bytes across process output streams.
    #[must_use]
    pub fn output_cap_bytes(&self) -> usize {
        self.output_cap_bytes
    }
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
            stdin: None,
            runtime_denies_subprocess: false,
        }
    }

    /// Attach an optional stdin payload to the command.
    #[must_use]
    pub fn with_stdin(mut self, stdin: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }

    /// Construct the sealed Node worker form understood by the Linux executor.
    /// The ordinary constructor cannot set the isolation marker: this validates
    /// that the stable permission model is mandatory and that no process,
    /// worker, native-addon, FFI, WASI, inspector, or network grant reopens it.
    pub fn node_permission_runtime(
        program: impl Into<std::path::PathBuf>,
        args: Vec<String>,
        cwd: impl Into<std::path::PathBuf>,
        origin: impl Into<String>,
    ) -> Result<Self, SandboxError> {
        let program = program.into();
        let permission = args.iter().any(|argument| argument == "--permission");
        let no_addons = args.iter().any(|argument| argument == "--no-addons");
        let forbidden = args.iter().any(|argument| {
            matches!(
                argument.as_str(),
                "--allow-child-process"
                    | "--allow-worker"
                    | "--allow-addons"
                    | "--allow-wasi"
                    | "--allow-ffi"
                    | "--allow-net"
                    | "--inspect"
                    | "--inspect-brk"
            ) || argument.starts_with("--inspect=")
                || argument.starts_with("--inspect-brk=")
        });
        if !program.is_absolute() || !permission || !no_addons || forbidden {
            return Err(SandboxError::InvalidCommand(
                "sealed Node UI runtime requires an absolute program, --permission and --no-addons, with no process/worker/native/network/inspector allowances"
                    .into(),
            ));
        }
        Ok(Self {
            program,
            args,
            cwd: cwd.into(),
            origin: origin.into(),
            stdin: None,
            runtime_denies_subprocess: true,
        })
    }

    #[must_use]
    fn runtime_denies_subprocess(&self) -> bool {
        self.runtime_denies_subprocess
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
    /// The backend cannot enforce a requested capability precisely. Refuse the
    /// run instead of silently widening it.
    #[error("sandbox backend cannot safely enforce requested capability: {0}")]
    UnsupportedCapability(String),
    /// Spawning the sandboxed process failed.
    #[error("spawning the sandboxed process failed: {0}")]
    Spawn(#[source] std::io::Error),
    /// An I/O error while running or reaping the process.
    #[error("sandbox I/O error: {0}")]
    Io(#[source] std::io::Error),

    /// The host revoked the command while it was running.
    #[error("sandbox command cancelled by the host")]
    Cancelled,
}

/// The enforcement seam: consume a [`SandboxProfile`] and run a command confined.
pub trait SandboxExecutor: Send + Sync {
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

    /// Run with a host-owned cancellation signal. Real enforcing backends
    /// override this to sweep the process group as soon as the signal changes.
    /// The default preserves compatibility for test doubles while still
    /// refusing a command already cancelled before launch.
    fn run_cancellable(
        &self,
        profile: &SandboxProfile,
        command: &SandboxCommand,
        cancelled: &AtomicBool,
    ) -> Result<SandboxOutcome, SandboxError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(SandboxError::Cancelled);
        }
        self.run(profile, command)
    }

    /// Lower a command into an enforcing, stdio-capable process specification.
    /// This is the only supported launch path for long-lived plugin protocols.
    /// It applies the same validation and backend policy as [`Self::run`].
    fn prepare_interactive(
        &self,
        profile: &SandboxProfile,
        command: &SandboxCommand,
    ) -> Result<SandboxProcessSpec, SandboxError>;
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

    fn prepare_interactive(
        &self,
        _profile: &SandboxProfile,
        _command: &SandboxCommand,
    ) -> Result<SandboxProcessSpec, SandboxError> {
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
    if profile.allow_subprocess {
        sb.push_str("(allow process-fork)\n");
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
    // Bubblewrap cannot express host:port allowlists. The executor rejects any
    // non-empty list before using this generator, so every admitted invocation
    // receives a private, disconnected network namespace.
    argv.push("--unshare-net".into());

    // Clean environment: clear, then re-add only the allowlisted vars that exist.
    argv.push("--clearenv".into());
    for name in &profile.env_allowlist {
        if let Some(value) = std::env::var_os(name) {
            argv.push("--setenv".into());
            argv.push(name.clone());
            argv.push(value.to_string_lossy().into_owned());
        }
    }

    // Base filesystem: a private /proc, /dev, and /tmp. Generic processes get
    // the traditional read-only system tree. A pinned host runtime that
    // independently denies process creation receives only shared-library roots,
    // not /bin, /sbin, /usr/bin, or all of /etc; this keeps ordinary executable
    // payloads absent from its mount namespace as an OS-level second layer.
    argv.push("--proc".into());
    argv.push("/proc".into());
    argv.push("--dev".into());
    argv.push("/dev".into());
    argv.push("--tmpfs".into());
    argv.push("/tmp".into());
    let system_roots: &[&str] = if command.runtime_denies_subprocess() {
        &["/lib", "/lib64", "/usr/lib", "/usr/lib64"]
    } else {
        &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"]
    };
    for sysdir in system_roots {
        argv.push("--ro-bind-try".into());
        argv.push((*sysdir).into());
        argv.push((*sysdir).into());
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

#[derive(Debug)]
struct SharedOutputBudget {
    remaining: usize,
}

/// Spawn `argv`, capture (capped) output, enforce the wall-clock kill on the whole
/// process group, and return the sanitized outcome. The environment is emptied and
/// only `env_allowlist` vars present in the parent are re-added.
#[allow(clippy::too_many_arguments)]
fn spawn_capture_kill(
    argv: &[String],
    cwd: &Path,
    env_allowlist: &[String],
    wall: Duration,
    output_cap_bytes: usize,
    origin: &str,
    backend: SandboxBackend,
    stdin: Option<&[u8]>,
    cancellation: Option<&AtomicBool>,
) -> Result<SandboxOutcome, SandboxError> {
    let Some((program, rest)) = argv.split_first() else {
        return Err(SandboxError::InvalidCommand("empty command".into()));
    };
    if cancellation.is_some_and(|signal| signal.load(Ordering::Acquire)) {
        return Err(SandboxError::Cancelled);
    }

    let (stdin_cfg, stdin_bytes) = match stdin {
        Some(bytes) => {
            if bytes.len() > MAX_STDIN_BYTES {
                return Err(SandboxError::InvalidCommand(format!(
                    "stdin payload is {} bytes, which exceeds the {} byte limit",
                    bytes.len(),
                    MAX_STDIN_BYTES
                )));
            }
            (Stdio::piped(), Some(bytes.to_vec()))
        }
        None => (Stdio::null(), None),
    };

    let mut cmd = Command::new(program);
    cmd.args(rest)
        .current_dir(cwd)
        // Clean environment: nothing inherited except the allowlisted vars.
        .env_clear()
        .stdin(stdin_cfg)
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

    // If stdin bytes are provided, write them from a dedicated thread to prevent deadlocks
    // and close stdin (by dropping the pipe) to signal EOF.
    if let Some(bytes) = stdin_bytes {
        if let Some(mut pipe) = child.stdin.take() {
            thread::spawn(move || {
                use std::io::Write as _;
                let _ = pipe.write_all(&bytes);
            });
        }
    }

    // Drain both pipes on their own threads so a chatty child never deadlocks on a
    // full pipe buffer, capping what is held in memory.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // The profile's output ceiling is aggregate across both streams. Giving
    // stdout and stderr separate budgets would retain up to twice the declared
    // amount and let an adversarial process amplify runner memory use.
    let output_budget = Arc::new(Mutex::new(SharedOutputBudget {
        remaining: output_cap_bytes,
    }));
    let stdout_budget = output_budget.clone();
    let stderr_budget = output_budget.clone();
    let out_handle = thread::spawn(move || read_capped(stdout, stdout_budget));
    let err_handle = thread::spawn(move || read_capped(stderr, stderr_budget));

    // Wait for exit, or kill the group on wall-clock expiry. A zero wall means no
    // wall-clock cap (the caller opted out); every profile derived from a manifest
    // carries a non-zero `wall_seconds`.
    let mut timed_out = false;
    let mut cancelled = false;
    let mut exit_code = None;
    let deadline = if wall.is_zero() {
        None
    } else {
        Some(started + wall)
    };
    loop {
        // Termination is detected *without* reaping (`waitid(WNOWAIT)`), so the
        // leader stays a zombie and its pid — the pgid — is still ours when the
        // sweep below runs. The old shape reaped first (`try_wait`) and swept
        // after, which could SIGKILL a recycled, unrelated process group.
        if child_terminated(&mut child, pid)? {
            // A successful direct child may still have left descendants holding
            // the capture pipes open; end the group before reaping, otherwise
            // the wall deadline stops being effective once the parent exits.
            #[cfg(unix)]
            kill_process_group(pid);
            exit_code = child.wait().map_err(SandboxError::Io)?.code();
            break;
        }
        if cancellation.is_some_and(|signal| signal.load(Ordering::Acquire)) {
            #[cfg(unix)]
            kill_process_group(pid);
            let _ = child.kill();
            let _ = child.wait();
            cancelled = true;
            break;
        }
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
    let duration = started.elapsed();

    // Killing the group closes the write ends, so the reader threads reach EOF.
    let (out_bytes, out_over) = out_handle.join().unwrap_or_else(|_| (Vec::new(), true));
    let (err_bytes, err_over) = err_handle.join().unwrap_or_else(|_| (Vec::new(), true));
    if cancelled {
        return Err(SandboxError::Cancelled);
    }

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

fn frozen_environment(env_allowlist: &[String]) -> Vec<(String, std::ffi::OsString)> {
    env_allowlist
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (name.clone(), value)))
        .collect()
}

/// Read a pipe fully, retaining only bytes left in the shared aggregate budget
/// and draining the rest to the void (so the child never blocks on a full
/// buffer). Returns the captured bytes and whether the cap was exceeded.
fn read_capped(
    reader: Option<impl Read>,
    budget: Arc<Mutex<SharedOutputBudget>>,
) -> (Vec<u8>, bool) {
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
                let take = {
                    let mut budget = budget
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let take = budget.remaining.min(n);
                    budget.remaining -= take;
                    take
                };
                buf.extend_from_slice(&chunk[..take]);
                if take < n {
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

/// SIGKILL every process in the group led by `pid` — a real `kill(-pgid,
/// SIGKILL)` syscall via `rustix` (which needs no `unsafe` here, so the
/// workspace-wide `unsafe_code = "deny"` still holds).
///
/// # The caller must not have reaped `pid` yet
///
/// A process group is named by its leader's pid. Once that leader is reaped the
/// kernel is free to recycle the pid, and this call would then SIGKILL a group
/// that belongs to somebody else. Sweep the group while the leader is either
/// still running or still an *unreaped zombie* — a zombie keeps its pid, and
/// therefore the pgid, reserved. [`child_terminated_unreaped`] is how a caller
/// learns the child is done without giving that reservation up.
///
/// This used to shell out to `/bin/kill`. That binary is absent on minimal
/// images (distroless, busybox-less containers), where the sweep became a
/// silent no-op and every grandchild leaked.
#[cfg(unix)]
pub fn kill_process_group(pid: u32) {
    let Ok(raw) = i32::try_from(pid) else {
        return;
    };
    let Some(pgid) = rustix::process::Pid::from_raw(raw) else {
        return;
    };
    let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL);
}

/// Whether child `pid` has terminated, **without reaping it** —
/// `waitid(P_PID, WEXITED | WNOWAIT | WNOHANG)`.
///
/// The child is left waitable (a zombie), so its pid — and therefore the pgid
/// of the group it leads — stays reserved and [`kill_process_group`] is still
/// safe to call. Reap afterwards with the normal `wait`, which returns at once.
///
/// `Some(true)` terminated, `Some(false)` still running, `None` when the state
/// could not be observed at all (not our child, or already reaped) — a caller
/// that gets `None` must fall back to its platform `try_wait`/`wait` rather
/// than assume either answer.
#[cfg(unix)]
pub fn child_terminated_unreaped(pid: u32) -> Option<bool> {
    let raw = i32::try_from(pid).ok()?;
    let pid = rustix::process::Pid::from_raw(raw)?;
    let options = rustix::process::WaitIdOptions::EXITED
        | rustix::process::WaitIdOptions::NOWAIT
        | rustix::process::WaitIdOptions::NOHANG;
    match rustix::process::waitid(rustix::process::WaitId::Pid(pid), options) {
        Ok(status) => Some(status.is_some()),
        Err(_) => None,
    }
}

/// Block until child `pid` terminates, **without reaping it** —
/// `waitid(P_PID, WEXITED | WNOWAIT)`.
///
/// The blocking twin of [`child_terminated_unreaped`], for a caller that owns a
/// dedicated thread and wants no polling at all. Returns `false` when the state
/// could not be observed (see that function); `true` when the child is a
/// waitable zombie and the group is still safe to sweep.
#[cfg(unix)]
pub fn await_child_terminated_unreaped(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    let Some(pid) = rustix::process::Pid::from_raw(raw) else {
        return false;
    };
    let options = rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT;
    loop {
        match rustix::process::waitid(rustix::process::WaitId::Pid(pid), options) {
            Ok(Some(_)) => return true,
            // A signal interrupted the wait: resume it rather than reporting a
            // termination that did not happen.
            Ok(None) => continue,
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => return false,
        }
    }
}

/// Whether the child has terminated, preferring the non-reaping probe on unix so
/// the caller can sweep the process group before the pid is released.
///
/// On a platform without `waitid` this falls back to `try_wait`, which *does*
/// reap — there the group sweep is `cfg`-ed out anyway.
fn child_terminated(child: &mut std::process::Child, pid: u32) -> Result<bool, SandboxError> {
    #[cfg(unix)]
    if let Some(terminated) = child_terminated_unreaped(pid) {
        return Ok(terminated);
    }
    let _ = pid;
    Ok(child.try_wait().map_err(SandboxError::Io)?.is_some())
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
        static NEVER_CANCELLED: AtomicBool = AtomicBool::new(false);
        self.run_cancellable(profile, command, &NEVER_CANCELLED)
    }

    fn run_cancellable(
        &self,
        profile: &SandboxProfile,
        command: &SandboxCommand,
        cancelled: &AtomicBool,
    ) -> Result<SandboxOutcome, SandboxError> {
        if !command.program.is_absolute() {
            return Err(SandboxError::InvalidCommand(format!(
                "program must be an absolute path, got `{}`",
                command.program.display()
            )));
        }
        validate_enforceable_profile(profile)?;
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
            command.stdin.as_deref(),
            Some(cancelled),
        )
    }

    fn prepare_interactive(
        &self,
        profile: &SandboxProfile,
        command: &SandboxCommand,
    ) -> Result<SandboxProcessSpec, SandboxError> {
        if !command.program.is_absolute() {
            return Err(SandboxError::InvalidCommand(format!(
                "program must be an absolute path, got `{}`",
                command.program.display()
            )));
        }
        validate_enforceable_profile(profile)?;
        let program = canonicalize_grant(&command.program.to_string_lossy());
        let cwd = std::path::PathBuf::from(canonicalize_grant(&command.cwd.to_string_lossy()));
        let mut resolved = profile.clone();
        resolved.read_paths = profile
            .read_paths
            .iter()
            .map(|path| canonicalize_grant(path))
            .collect();
        resolved.write_paths = profile
            .write_paths
            .iter()
            .map(|path| canonicalize_grant(path))
            .collect();
        let sandbox = seatbelt_profile(&resolved, Path::new(&program));
        let mut argv = vec![
            self.tool.to_string_lossy().into_owned(),
            "-p".into(),
            sandbox,
            program,
        ];
        argv.extend(command.args.iter().cloned());
        Ok(SandboxProcessSpec {
            backend: SandboxBackend::Seatbelt,
            argv,
            cwd,
            environment: frozen_environment(&profile.env_allowlist),
            origin: command.origin.clone(),
            wall_clock: Duration::from_secs(profile.wall_seconds),
            output_cap_bytes: output_cap_bytes(profile),
        })
    }
}

/// Resolve `path` to its canonical (symlink-free) form so a Seatbelt `(subpath …)`
/// rule matches the kernel's resolved path.
///
/// Walks the components and canonicalizes each prefix that exists, so a
/// symlink is resolved at **any** position — including after a `..` that pops
/// back into existing territory. (The kernel resolves left-to-right, so a plain
/// `canonicalize` dies at the first missing component; a naive ancestor walk
/// that re-appends the remainder lexically leaves symlinks after the pop
/// unresolved, which would emit a `(subpath …)` rule the kernel's resolved path
/// does not match. Same shape as `policy::canonicalize_lenient` in
/// `codypendent-daemon` — the sandbox crate cannot depend on it, so the walk
/// is duplicated here.)
#[cfg(target_os = "macos")]
fn canonicalize_grant(path: &str) -> String {
    let p = Path::new(path);
    if let Ok(canon) = std::fs::canonicalize(p) {
        return canon.to_string_lossy().into_owned();
    }
    let mut resolved = std::path::PathBuf::new();
    for component in p.components() {
        match component {
            std::path::Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            std::path::Component::RootDir => {
                resolved.push(std::path::Component::RootDir.as_os_str())
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
                if resolved.exists() {
                    if let Ok(canon) = std::fs::canonicalize(&resolved) {
                        resolved = canon;
                    }
                }
            }
            std::path::Component::Normal(segment) => {
                resolved.push(segment);
                if resolved.exists() {
                    if let Ok(canon) = std::fs::canonicalize(&resolved) {
                        resolved = canon;
                    }
                }
            }
        }
    }
    // Seatbelt subpaths must be absolute; a relative input whose prefix never
    // resolved is anchored at the daemon's cwd rather than handed out raw.
    if !resolved.is_absolute() {
        if let Ok(cwd) = std::env::current_dir() {
            resolved = cwd.join(resolved);
        }
    }
    resolved.to_string_lossy().into_owned()
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
        let executor = Self { tool };
        // A binary on PATH does not establish that this kernel permits user
        // namespaces (containers commonly deny them). Exercise the same
        // confinement path with a fixed, side-effect-free command before
        // advertising an available executor. The normal wall/output bounds
        // also apply to the probe; a failure never falls back to host exec.
        let profile = SandboxProfile::new(
            "codypendent-sandbox-probe",
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            true,
            128,
            1,
            2,
            1,
        );
        let probe = SandboxCommand::new("/bin/true", vec![], "/", "sandbox-probe");
        match executor.run(&profile, &probe) {
            Ok(outcome) if outcome.success() => Ok(executor),
            _ => Err(SandboxError::ToolUnavailable {
                tool: "bwrap".into(),
                diagnostic: "bubblewrap confinement probe failed; user namespaces or required kernel capabilities are unavailable".into(),
            }),
        }
    }
}

#[cfg(target_os = "linux")]
impl SandboxExecutor for LinuxSandbox {
    fn capability_report(&self) -> CapabilityReport {
        let available = is_executable_file(&self.tool);
        let mut degraded = vec![
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
        static NEVER_CANCELLED: AtomicBool = AtomicBool::new(false);
        self.run_cancellable(profile, command, &NEVER_CANCELLED)
    }

    fn run_cancellable(
        &self,
        profile: &SandboxProfile,
        command: &SandboxCommand,
        cancelled: &AtomicBool,
    ) -> Result<SandboxOutcome, SandboxError> {
        if !command.program.is_absolute() {
            return Err(SandboxError::InvalidCommand(format!(
                "program must be an absolute path, got `{}`",
                command.program.display()
            )));
        }
        validate_enforceable_profile(profile)?;
        if !profile.allow_subprocess && !command.runtime_denies_subprocess() {
            return Err(SandboxError::UnsupportedCapability(
                "Linux bubblewrap cannot prevent exec of bound system binaries when subprocess=false"
                    .into(),
            ));
        }
        // `bwrap_argv` names `bwrap`/`prlimit` bare; resolve `bwrap` to the probed
        // absolute path so the spawn does not depend on PATH ordering.
        let mut argv = bwrap_argv(profile, command);
        if let Some(first) = argv.iter_mut().find(|a| a.as_str() == "bwrap") {
            *first = self.tool.to_string_lossy().into_owned();
        }
        if let Some(first) = argv.iter_mut().find(|a| a.as_str() == "prlimit") {
            *first = locate_on_path("prlimit")
                .ok_or_else(|| SandboxError::ToolUnavailable {
                    tool: "prlimit".into(),
                    diagnostic: "memory/CPU caps require an absolute prlimit executable".into(),
                })?
                .to_string_lossy()
                .into_owned();
        }
        spawn_capture_kill(
            &argv,
            &command.cwd,
            &profile.env_allowlist,
            Duration::from_secs(profile.wall_seconds),
            output_cap_bytes(profile),
            &command.origin,
            SandboxBackend::Bubblewrap,
            command.stdin.as_deref(),
            Some(cancelled),
        )
    }

    fn prepare_interactive(
        &self,
        profile: &SandboxProfile,
        command: &SandboxCommand,
    ) -> Result<SandboxProcessSpec, SandboxError> {
        if !command.program.is_absolute() {
            return Err(SandboxError::InvalidCommand(format!(
                "program must be an absolute path, got `{}`",
                command.program.display()
            )));
        }
        validate_enforceable_profile(profile)?;
        if !profile.allow_subprocess && !command.runtime_denies_subprocess() {
            return Err(SandboxError::UnsupportedCapability(
                "Linux bubblewrap cannot prevent exec of bound system binaries when subprocess=false"
                    .into(),
            ));
        }
        let mut argv = bwrap_argv(profile, command);
        if let Some(first) = argv.iter_mut().find(|value| value.as_str() == "bwrap") {
            *first = self.tool.to_string_lossy().into_owned();
        }
        if let Some(first) = argv.iter_mut().find(|value| value.as_str() == "prlimit") {
            *first = locate_on_path("prlimit")
                .ok_or_else(|| SandboxError::ToolUnavailable {
                    tool: "prlimit".into(),
                    diagnostic: "memory/CPU caps require an absolute prlimit executable".into(),
                })?
                .to_string_lossy()
                .into_owned();
        }
        Ok(SandboxProcessSpec {
            backend: SandboxBackend::Bubblewrap,
            argv,
            cwd: command.cwd.clone(),
            environment: frozen_environment(&profile.env_allowlist),
            origin: command.origin.clone(),
            wall_clock: Duration::from_secs(profile.wall_seconds),
            output_cap_bytes: output_cap_bytes(profile),
        })
    }
}

fn validate_enforceable_profile(profile: &SandboxProfile) -> Result<(), SandboxError> {
    if !profile.network_allowlist.is_empty() {
        return Err(SandboxError::UnsupportedCapability(
            "host:port network allowlists require a broker; refusing unrestricted outbound access"
                .into(),
        ));
    }
    for (field, value) in [
        ("memory_mb", profile.memory_mb),
        ("cpu_seconds", profile.cpu_seconds),
        ("wall_seconds", profile.wall_seconds),
        ("maximum_output_mb", profile.maximum_output_mb),
    ] {
        if value == 0 {
            return Err(SandboxError::InvalidCommand(format!(
                "resource cap `{field}` must be greater than zero"
            )));
        }
    }
    if profile
        .read_paths
        .iter()
        .chain(profile.write_paths.iter())
        .any(|path| path.trim_end_matches('/').is_empty())
    {
        return Err(SandboxError::InvalidCommand(
            "root/empty filesystem grants are forbidden".into(),
        ));
    }
    Ok(())
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

    /// B1-family regression for the Seatbelt grant canonicalizer: the kernel
    /// resolves left-to-right, so `<root>/nope/../link/…` dies at `nope` under
    /// a plain `canonicalize`. The old ancestor walk re-appended the remainder
    /// lexically and emitted a `(subpath …)` rule the kernel's resolved path
    /// would not match; the component walk must resolve `link`.
    #[cfg(target_os = "macos")]
    #[test]
    fn canonicalize_grant_resolves_a_symlink_after_a_pop() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        // Canonicalize before linking: on macOS `/var` is itself a symlink,
        // and the grant must come back in the kernel's canonical spelling.
        let outside = tempfile::tempdir().unwrap();
        let outside = std::fs::canonicalize(outside.path()).unwrap();
        let link = root.join("link");
        symlink(&outside, &link).unwrap();

        let sneaky = root.join("nope/../link/target");
        let grant = canonicalize_grant(&sneaky.to_string_lossy());
        assert_eq!(grant, outside.join("target").to_string_lossy());
    }

    /// The window the group sweep has to live inside.
    ///
    /// `waitid(WNOWAIT)` reports the child as finished while leaving it a
    /// zombie, so its pid — which is the pgid of the group it leads — is still
    /// *reserved*: the kernel cannot hand that pgid to anybody else, and a
    /// surviving descendant in the group is still reachable. Reaping releases
    /// the pid, and the same probe can no longer see the child at all: that is
    /// the moment after which a group kill may land on whatever unrelated
    /// process group inherits the pid.
    ///
    /// The old order (`try_wait` first, sweep second) put every sweep on the
    /// wrong side of that line.
    #[cfg(unix)]
    #[test]
    fn a_finished_child_keeps_its_group_sweepable_until_it_is_reaped() {
        use std::os::unix::process::CommandExt as _;

        // The leader exits at once and leaves a live grandchild in its group —
        // precisely the case the post-exit sweep exists for.
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30 & exit 7")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn group leader");
        let pid = child.id();
        let raw = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();

        // Wait for the leader to finish *without* reaping it.
        let deadline = Instant::now() + Duration::from_secs(10);
        while child_terminated_unreaped(pid) != Some(true) {
            assert!(Instant::now() < deadline, "child never finished");
            thread::sleep(Duration::from_millis(5));
        }

        // Unreaped: the group is still ours, and the sweep still reaches the
        // grandchild the exited leader left behind.
        assert!(
            rustix::process::test_kill_process_group(raw).is_ok(),
            "the group must still be addressable before the reap"
        );
        kill_process_group(pid);

        // The exit status is still there to collect, because nothing reaped it
        // early. Reap it HERE, before asserting the group is gone: on Linux
        // `kill(-pgid, 0)` succeeds while ANY member exists, and an unreaped
        // zombie leader is a member — so the group stays addressable on the
        // leader's account no matter what happened to the grandchild, and the
        // wait below would spin until it timed out. (macOS does not count the
        // zombie, which is why this only ever failed on the Linux runner.)
        // Reaping first leaves the grandchild as the only thing that could
        // still hold the group open, so the loop tests what it claims to.
        assert_eq!(child.wait().expect("reap").code(), Some(7));

        let deadline = Instant::now() + Duration::from_secs(10);
        while process_group_has_live_members(pid) {
            assert!(
                Instant::now() < deadline,
                "the grandchild outlived the sweep"
            );
            thread::sleep(Duration::from_millis(5));
        }

        // Reaped: the pid is released and the probe can no longer see the child.
        // Any sweep from here on is aimed at whatever inherits the pid next —
        // which is why no caller may issue one.
        assert_eq!(
            child_terminated_unreaped(pid),
            None,
            "a reaped pid must not be reported as an observable child"
        );
    }
    // Minimal container PID 1 implementations may leave killed orphaned
    // grandchildren as zombies indefinitely. kill(pgid, 0) counts those dead
    // processes too; the invariant here is that no executable member survives.
    #[cfg(target_os = "linux")]
    fn process_group_has_live_members(group: u32) -> bool {
        std::fs::read_dir("/proc")
            .expect("read process table")
            .any(|entry| {
                let Ok(entry) = entry else {
                    return false;
                };
                if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
                    return false;
                }
                let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                    return false;
                };
                let Some((_, fields)) = stat.rsplit_once(") ") else {
                    return false;
                };
                let fields: Vec<_> = fields.split_whitespace().collect();
                fields.get(2).and_then(|value| value.parse::<u32>().ok()) == Some(group)
                    && fields
                        .first()
                        .is_some_and(|state| *state != "Z" && *state != "X")
            })
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    fn process_group_has_live_members(group: u32) -> bool {
        let raw = rustix::process::Pid::from_raw(i32::try_from(group).unwrap()).unwrap();
        rustix::process::test_kill_process_group(raw).is_ok()
    }

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
memory_mb = 128
cpu_seconds = 30
wall_seconds = 30
maximum_output_mb = 8
"#,
        );
        let cmd = SandboxCommand::new("/bin/cat", vec!["/work/f".into()], "/work", "plugin:x");
        let argv = bwrap_argv(&p, &cmd);
        assert!(argv.iter().any(|arg| arg == "bwrap"));
        assert!(argv.iter().any(|a| a == "--unshare-net"));
        assert!(argv.iter().any(|a| a == "--clearenv"));
        assert!(argv
            .windows(3)
            .any(|w| w == ["--ro-bind", "/work", "/work"]));
        // program + args come after the `--` separator, in order.
        let sep = argv.iter().rposition(|a| a == "--").unwrap();
        assert_eq!(argv[sep + 1], "/bin/cat");
        assert_eq!(argv[sep + 2], "/work/f");
    }

    #[test]
    fn bwrap_argv_keeps_network_unshared_and_prefixes_prlimit_with_caps() {
        let p = sample_profile(); // network non-empty, memory 256, cpu 60
        let cmd = SandboxCommand::new("/bin/echo", vec![], "/workspace/repo", "plugin:github");
        let argv = bwrap_argv(&p, &cmd);
        // rlimit caps ⇒ prlimit prefix before bwrap.
        assert_eq!(argv.first().map(String::as_str), Some("prlimit"));
        assert!(argv.iter().any(|a| a == "--cpu=60"));
        assert!(argv
            .iter()
            .any(|a| a == &format!("--as={}", 256u64 * 1024 * 1024)));
        // The pure argv stays deny-all. The executor refuses a non-empty
        // host:port allowlist until a broker can enforce it precisely.
        assert!(argv.iter().any(|a| a == "--unshare-net"));
        // write path is a read-write bind.
        assert!(argv
            .windows(3)
            .any(|w| w == ["--bind", "/workspace/repo/target", "/workspace/repo/target"]));
    }

    #[test]
    fn host_runtime_without_subprocess_excludes_system_binary_trees() {
        let p = sample_profile();
        let cmd = SandboxCommand::node_permission_runtime(
            "/opt/codypendent/node/bin/node",
            vec![
                "--permission".into(),
                "--no-addons".into(),
                "/package/worker.mjs".into(),
            ],
            "/package",
            "ui-component:test",
        )
        .unwrap();
        let argv = bwrap_argv(&p, &cmd);
        for executable_root in ["/bin", "/sbin", "/usr"] {
            assert!(
                !argv.windows(3).any(|window| {
                    window[0] == "--ro-bind-try"
                        && window[1] == executable_root
                        && window[2] == executable_root
                }),
                "{executable_root} must be absent from the UI worker namespace"
            );
        }
        assert!(argv.windows(3).any(|window| {
            window[0] == "--ro-bind-try" && window[1] == "/usr/lib" && window[2] == "/usr/lib"
        }));
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
    fn stdout_and_stderr_share_one_output_budget() {
        let budget = Arc::new(Mutex::new(SharedOutputBudget { remaining: 10 }));
        let (stdout, stdout_overflowed) =
            read_capped(Some(std::io::Cursor::new(vec![b'o'; 8])), budget.clone());
        let (stderr, stderr_overflowed) =
            read_capped(Some(std::io::Cursor::new(vec![b'e'; 8])), budget);

        assert_eq!(stdout.len() + stderr.len(), 10);
        assert!(!stdout_overflowed);
        assert!(stderr_overflowed);
    }

    #[test]
    fn stdin_payload_is_delivered_to_the_child() {
        #[cfg(unix)]
        {
            let cat = if Path::new("/bin/cat").exists() {
                "/bin/cat"
            } else {
                "/usr/bin/cat"
            };
            let payload = b"hello from stdin\n";
            let outcome = spawn_capture_kill(
                &[cat.to_string()],
                Path::new("/tmp"),
                &[],
                Duration::from_secs(5),
                1024 * 1024,
                "test:stdin",
                SandboxBackend::None,
                Some(payload),
                None,
            )
            .expect("cat should succeed");
            assert_eq!(outcome.exit_code, Some(0));
            assert_eq!(outcome.stdout.text, "hello from stdin\n");
        }
    }

    #[test]
    fn child_that_ignores_stdin_does_not_error() {
        #[cfg(unix)]
        {
            let echo = if Path::new("/bin/echo").exists() {
                "/bin/echo"
            } else {
                "/usr/bin/echo"
            };
            let payload = vec![b'a'; 8192];
            let outcome = spawn_capture_kill(
                &[echo.to_string(), "done".to_string()],
                Path::new("/tmp"),
                &[],
                Duration::from_secs(5),
                1024 * 1024,
                "test:stdin_ignore",
                SandboxBackend::None,
                Some(&payload),
                None,
            )
            .expect("echo should succeed even if stdin is ignored");
            assert_eq!(outcome.exit_code, Some(0));
            assert_eq!(outcome.stdout.text.trim(), "done");
        }
    }

    #[test]
    fn oversized_stdin_is_refused_not_truncated() {
        let oversized = vec![0u8; MAX_STDIN_BYTES + 1];
        let err = spawn_capture_kill(
            &["/bin/cat".to_string()],
            Path::new("/tmp"),
            &[],
            Duration::from_secs(5),
            1024 * 1024,
            "test:oversized",
            SandboxBackend::None,
            Some(&oversized),
            None,
        )
        .unwrap_err();
        match err {
            SandboxError::InvalidCommand(msg) => {
                assert!(msg.contains("exceeds the"));
                assert!(msg.contains("byte limit"));
            }
            other => panic!("expected InvalidCommand, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn host_cancellation_kills_the_running_process_group() {
        let sleep = if Path::new("/bin/sleep").exists() {
            "/bin/sleep"
        } else {
            "/usr/bin/sleep"
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let setter = cancelled.clone();
        let signal_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            setter.store(true, Ordering::Release);
        });
        let started = Instant::now();

        let error = spawn_capture_kill(
            &[sleep.to_string(), "30".to_string()],
            Path::new("/tmp"),
            &[],
            Duration::from_secs(30),
            1024,
            "test:cancel",
            SandboxBackend::None,
            None,
            Some(cancelled.as_ref()),
        )
        .unwrap_err();
        signal_thread.join().unwrap();

        assert!(matches!(error, SandboxError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn sandbox_command_is_structured() {
        let cmd = SandboxCommand::new("/bin/echo", vec!["hi".into()], "/tmp", "plugin:x");
        assert_eq!(cmd.program, Path::new("/bin/echo"));
        assert_eq!(cmd.args, ["hi"]);
        assert_eq!(cmd.origin, "plugin:x");
        assert!(cmd.stdin.is_none());
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
