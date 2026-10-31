//! Starting `codypendentd` from the desktop shell.
//!
//! The shell used to connect to the socket and nothing else. On a machine
//! where no daemon is running — every first launch — the operator got a
//! banner quoting `No such file or directory (os error 2)` and a reconnect
//! loop that would never succeed, because nothing anywhere told them a daemon
//! had to be started, let alone started it. The CLI has done this for itself
//! since Phase 1 (`crates/cli/src/commands.rs::ensure_daemon`): spawn the
//! daemon detached, log it to `<data_dir>/logs/daemon.log`, wait for the
//! socket. This module is that flow for the desktop, with one difference: the
//! desktop binary is not `codypendent`, so the daemon program has to be found
//! rather than assumed to be `current_exe`.
//!
//! Nothing here is privileged beyond what the operator could type in a
//! terminal — `codypendent daemon start` — and the resolution never guesses:
//! when no program can be found the outcome names exactly what was looked for
//! and the command to run by hand.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{read_envelope, write_envelope, ClientId, Envelope, Payload};
use serde::Serialize;
use tokio::net::UnixStream;

/// Cold startup also recovers the database and verifies the bundled runtime.
/// Keep the launch pending while that work completes, with a bounded wait.
const START_BUDGET: Duration = Duration::from_secs(90);
/// How often to poll the socket while waiting.
const START_POLL: Duration = Duration::from_millis(100);

/// How long one `Ping`/`Pong` exchange may take before the socket counts as
/// unanswered. Generous for a local socket, and well inside [`START_BUDGET`]
/// so a wedged peer still leaves room for the poll loop to give up.
const PING_BUDGET: Duration = Duration::from_secs(2);
static START_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The daemon program the shell would launch, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonInvocation {
    /// The executable, as resolved.
    pub program: String,
    /// Arguments after the program: `["__daemon"]` for the single binary,
    /// nothing for a standalone `codypendentd`.
    pub args: Vec<String>,
}

/// Where a daemon program was found: the operator's override, the `PATH`, or
/// one of the directories the installer uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InvocationSource {
    Override,
    Path,
    KnownDirectory,
    /// Inside the same application bundle, including Linux runtime resources.
    BesideThisApp,
    DevelopmentBuild,
}

/// What `daemon_launch_status` answers with: whether anything is listening,
/// what the shell would launch, and what to type if it cannot.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchStatus {
    pub socket_path: String,
    /// Whether a daemon answered a `Ping` on that socket just now.
    pub listening: bool,
    /// The program the shell would run, when one was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invocation: Option<DaemonInvocation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<InvocationSource>,
    /// The command to run in a terminal instead. Always present, so the UI can
    /// always name it.
    pub manual_command: String,
    /// Where the spawned daemon's output goes.
    pub log_path: String,
    /// Every location that was checked and had nothing, when none had it.
    pub searched: Vec<String>,
}

/// What `daemon_start` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum StartOutcome {
    /// A daemon was already answering; nothing was spawned.
    AlreadyRunning,
    /// A daemon was spawned and answered within the budget.
    Started { pid: u32, program: String },
}

/// The environment variable an operator sets to name the daemon program
/// explicitly — a development build, a non-standard install directory.
pub const PROGRAM_OVERRIDE_ENV: &str = "CODYPENDENT_BINARY";

/// The command shown to the operator when the shell cannot start the daemon
/// itself. `codypendent daemon start` is the documented human-facing verb
/// (`docs/cli-and-tui-user-guide.md` §2).
pub const MANUAL_COMMAND: &str = "codypendent daemon start";

/// True when a daemon answers `Ping` with `Pong` on this socket. The CLI's
/// `client::ping`, verbatim: a `DaemonStatus` request would be richer, but
/// "is anything there" is the only question the launcher asks.
pub async fn ping(socket: &Path) -> bool {
    // The WHOLE exchange is bounded. A wedged peer can accept the connection
    // and never answer, and an unbounded read there would hang
    // `daemon_launch_status` and every poll in `start_daemon` forever — the
    // start budget below would never elapse, and the disconnected banner
    // would sit pending in exactly the unresponsive-daemon case it exists to
    // recover from. A timeout is indistinguishable from "nothing there",
    // which is the only question this asks.
    ping_within(socket, PING_BUDGET).await
}

/// [`ping`] with the bound named, so a test can assert the timeout without
/// waiting out the real budget.
async fn ping_within(socket: &Path, budget: Duration) -> bool {
    tokio::time::timeout(budget, async {
        let Ok(mut stream) = UnixStream::connect(socket).await else {
            return false;
        };
        let request = Envelope::request(ClientId::new(), Payload::Ping);
        if write_envelope(&mut stream, &request).await.is_err() {
            return false;
        }
        matches!(
            read_envelope(&mut stream).await,
            Ok(Some(Envelope {
                payload: Payload::Pong,
                ..
            }))
        )
    })
    .await
    .unwrap_or(false)
}

/// The directories an installer puts `codypendent` in, tried when `PATH` does
/// not have it. A bundled `.app` launched from Finder inherits a minimal
/// `PATH` that has none of them, which is exactly when this list matters.
fn known_directories() -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        directories.push(home.join(".cargo").join("bin"));
        directories.push(home.join(".local").join("bin"));
    }
    directories.push(PathBuf::from("/usr/local/bin"));
    directories.push(PathBuf::from("/opt/homebrew/bin"));
    directories.push(PathBuf::from("/opt/codypendent/bin"));
    directories
}

/// Finder supplies a minimal PATH. Keep the user's explicit order, then add
/// installed tool locations without evaluating a shell profile (which can
/// prompt, perform network work, or never finish).
fn daemon_path() -> Option<std::ffi::OsString> {
    let mut directories: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    let mut candidates = known_directories();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for relative in [
            ".local/share/mise/shims",
            ".asdf/shims",
            ".pyenv/shims",
            ".bun/bin",
            ".volta/bin",
        ] {
            candidates.push(home.join(relative));
        }
    }
    for directory in candidates {
        if directory.is_dir() && !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    std::env::join_paths(directories).ok()
}

/// The program names to look for, in order: the single binary first (it runs
/// a daemon that always matches its own protocol), then the standalone daemon.
const PROGRAM_NAMES: [&str; 2] = ["codypendent", "codypendentd"];

fn invocation_for(program: &Path) -> DaemonInvocation {
    let is_single_binary = program
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem == "codypendent");
    DaemonInvocation {
        program: program.display().to_string(),
        args: if is_single_binary {
            vec!["__daemon".to_owned()]
        } else {
            Vec::new()
        },
    }
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Everything the resolver consults, so the pure resolution below can be
/// exercised with a fake environment.
struct Environment {
    override_program: Option<PathBuf>,
    path_entries: Vec<PathBuf>,
    known_directories: Vec<PathBuf>,
    beside_app: Option<PathBuf>,
    packaged_runtime: Option<PathBuf>,
    development_build: Option<PathBuf>,
}

impl Environment {
    fn from_process() -> Self {
        let beside_app = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        Self {
            override_program: std::env::var_os(PROGRAM_OVERRIDE_ENV)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            path_entries: std::env::var_os("PATH")
                .map(|path| std::env::split_paths(&path).collect())
                .unwrap_or_default(),
            known_directories: known_directories(),
            packaged_runtime: beside_app.as_deref().and_then(packaged_runtime_directory),
            beside_app,
            development_build: cfg!(debug_assertions)
                .then(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/debug")),
        }
    }
}

/// Match the pinned Tauri Linux `resource_dir` convention. Tauri codegen uses
/// `productName` for PackageInfo.name (not the binary's file name), and the
/// installed/AppImage resource tree is `usr/lib/<name>` beside `usr/bin`.
/// Debug window-only builds have no such tree and retain ordinary discovery.
fn packaged_runtime_directory(beside_app: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        // The package layout identifies ownership even when every resource was
        // deleted. Checking directory existence would let a damaged AppImage
        // silently switch to an installed CLI. Cargo's target/debug layout is
        // deliberately outside this condition.
        if !beside_app.ends_with(Path::new("usr/bin")) {
            return None;
        }
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).ok()?;
        let product = config
            .get("productName")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(env!("CARGO_PKG_NAME"));
        Some(
            beside_app
                .parent()?
                .join("lib")
                .join(product)
                .join("runtime"),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = beside_app;
        None
    }
}

/// Resolve the daemon program, returning what was found (and where), or every
/// location that was checked.
fn resolve_in(
    environment: &Environment,
    executable: &dyn Fn(&Path) -> bool,
) -> Result<(DaemonInvocation, InvocationSource), Vec<String>> {
    let mut searched = Vec::new();
    if let Some(program) = &environment.override_program {
        if executable(program) {
            return Ok((invocation_for(program), InvocationSource::Override));
        }
        searched.push(format!("{PROGRAM_OVERRIDE_ENV}={}", program.display()));
    }
    if let Some(runtime) = &environment.packaged_runtime {
        let program = runtime.join("codypendent");
        if executable(&program) {
            return Ok((invocation_for(&program), InvocationSource::BesideThisApp));
        }
        // A damaged package must report its missing runtime instead of silently
        // starting an unrelated, possibly older installation from PATH.
        searched.push(program.display().to_string());
        return Err(searched);
    }
    // A packaged app must start the daemon it shipped with, even when an
    // older CLI is also on PATH. The explicit development override still wins.
    if let Some(beside) = &environment.beside_app {
        for name in PROGRAM_NAMES {
            let candidate = beside.join(name);
            if executable(&candidate) {
                return Ok((invocation_for(&candidate), InvocationSource::BesideThisApp));
            }
            searched.push(candidate.display().to_string());
        }
    }
    if let Some(directory) = &environment.development_build {
        for name in PROGRAM_NAMES {
            let candidate = directory.join(name);
            if executable(&candidate) {
                return Ok((
                    invocation_for(&candidate),
                    InvocationSource::DevelopmentBuild,
                ));
            }
            searched.push(candidate.display().to_string());
        }
    }
    let candidates = |directories: &[PathBuf]| {
        let mut found = Vec::new();
        for directory in directories {
            for name in PROGRAM_NAMES {
                found.push(directory.join(name));
            }
        }
        found
    };
    for (directories, source) in [
        (&environment.path_entries, InvocationSource::Path),
        (
            &environment.known_directories,
            InvocationSource::KnownDirectory,
        ),
    ] {
        for candidate in candidates(directories) {
            if executable(&candidate) {
                return Ok((invocation_for(&candidate), source));
            }
            searched.push(candidate.display().to_string());
        }
    }
    Err(searched)
}

fn resolve() -> Result<(DaemonInvocation, InvocationSource), Vec<String>> {
    resolve_in(&Environment::from_process(), &is_executable)
}

fn log_path(paths: &RuntimePaths) -> PathBuf {
    paths.log_dir.join("daemon.log")
}

/// Whether a daemon is listening, and what the shell would do about it if not.
pub async fn launch_status(paths: &RuntimePaths) -> LaunchStatus {
    let listening = ping(&paths.socket_path).await;
    let (invocation, source, searched) = match resolve() {
        Ok((invocation, source)) => (Some(invocation), Some(source), Vec::new()),
        Err(searched) => (None, None, searched),
    };
    LaunchStatus {
        socket_path: paths.socket_path.display().to_string(),
        listening,
        invocation,
        source,
        manual_command: MANUAL_COMMAND.to_owned(),
        log_path: log_path(paths).display().to_string(),
        searched,
    }
}

/// Start a daemon unless one is already answering, then wait for the socket.
///
/// The spawn is detached — its own process group, stdin closed, output to the
/// daemon log — so it outlives this window, exactly as the CLI's does. An
/// error names what was tried and the command to run by hand.
pub async fn start_daemon(paths: &RuntimePaths) -> anyhow::Result<StartOutcome> {
    let _starting = START_LOCK.lock().await;
    if ping(&paths.socket_path).await {
        return Ok(StartOutcome::AlreadyRunning);
    }
    let (invocation, _) = resolve().map_err(|searched| {
        anyhow::anyhow!(
            "no codypendent program was found to start the daemon with. Looked for \
             `codypendent` and `codypendentd` on PATH and in {}. Install Codypendent \
             (see install.sh), set {PROGRAM_OVERRIDE_ENV} to the binary, or run \
             `{MANUAL_COMMAND}` in a terminal.",
            searched
                .iter()
                .filter(|entry| entry.ends_with("/codypendent"))
                .map(|entry| entry.trim_end_matches("/codypendent").to_owned())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    paths
        .ensure_directories()
        .context("creating the codypendent data directories")?;
    let log = log_path(paths);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("opening {}", log.display()))?;
    let log_for_stderr = log_file
        .try_clone()
        .with_context(|| format!("opening {}", log.display()))?;

    let mut command = std::process::Command::new(&invocation.program);
    crate::runtime_paths::configure_command(&mut command, paths);
    if let Some(path) = daemon_path() {
        command.env("PATH", path);
    }
    command
        .args(&invocation.args)
        .stdin(std::process::Stdio::null())
        .stdout(log_file)
        .stderr(log_for_stderr);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Its own process group: the daemon must not die with this window.
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}", invocation.program))?;
    let pid = child.id();

    if wait_for_startup(&paths.socket_path, &mut child, START_BUDGET)
        .await
        .with_context(|| format!("starting {}; check {}", invocation.program, log.display()))?
    {
        return Ok(StartOutcome::Started {
            pid,
            program: invocation.program,
        });
    }
    bail!(
        "{} (pid {pid}) was started but nothing answered on {} within {} seconds; check {}",
        invocation.program,
        paths.socket_path.display(),
        START_BUDGET.as_secs(),
        log.display()
    )
}

async fn wait_for_startup(
    socket: &Path,
    child: &mut std::process::Child,
    budget: Duration,
) -> anyhow::Result<bool> {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        // The daemon binds before recovery, then accepts after initialization.
        // Keep this readiness connection alive until the startup deadline;
        // dropping it every two seconds leaves closed peers in the accept queue.
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let ready = ping_within(socket, remaining);
        tokio::pin!(ready);
        let answered = loop {
            tokio::select! {
                answered = &mut ready => break answered,
                _ = tokio::time::sleep(START_POLL) => ensure_still_starting(child)?,
            }
        };
        if answered {
            return Ok(true);
        }
        ensure_still_starting(child)?;
        tokio::time::sleep(START_POLL).await;
    }
    Ok(false)
}

fn ensure_still_starting(child: &mut std::process::Child) -> anyhow::Result<()> {
    if let Some(status) = child.try_wait().context("checking daemon startup")? {
        bail!("daemon exited with {status} before the workspace was ready");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    struct StartupChild(std::process::Child);

    impl Drop for StartupChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn startup_child(script: &str) -> StartupChild {
        StartupChild(
            std::process::Command::new("/bin/sh")
                .args(["-c", script])
                .spawn()
                .expect("start fixture child"),
        )
    }

    #[tokio::test]
    async fn startup_keeps_one_readiness_connection_while_the_child_initializes() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("starting.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_envelope(&mut stream).await.unwrap().unwrap();
            tokio::time::sleep(START_POLL * 2).await;
            write_envelope(&mut stream, &Envelope::reply_to(&request, Payload::Pong))
                .await
                .unwrap();
        });
        let mut child = startup_child("exec sleep 30");
        assert!(
            wait_for_startup(&socket, &mut child.0, Duration::from_secs(1))
                .await
                .unwrap()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn startup_remains_bounded_when_a_live_child_never_answers() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("silent.sock");
        let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let mut child = startup_child("exec sleep 30");
        let started = std::time::Instant::now();
        assert!(
            !wait_for_startup(&socket, &mut child.0, Duration::from_millis(150))
                .await
                .unwrap()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn startup_reports_a_child_exit_without_waiting_for_the_socket_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("exited.sock");
        let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let mut child = startup_child("exit 7");
        assert_eq!(child.0.wait().unwrap().code(), Some(7));
        let started = std::time::Instant::now();
        let error = wait_for_startup(&socket, &mut child.0, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("7"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// A peer that ACCEPTS the connection and then goes silent must not hang
    /// the caller. `daemon_launch_status` and every poll in `start_daemon`
    /// await this; an unbounded read there would strand the disconnected
    /// banner pending in exactly the wedged-daemon case it exists to recover
    /// from.
    #[tokio::test]
    async fn a_socket_that_accepts_and_never_answers_fails_within_its_budget() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("silent.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        // Accept and hold the connection open, answering nothing.
        let held = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(stream);
        });

        let started = std::time::Instant::now();
        let answered = ping_within(&socket, Duration::from_millis(150)).await;
        let elapsed = started.elapsed();

        assert!(!answered, "a silent peer is not a running daemon");
        assert!(
            elapsed < Duration::from_secs(5),
            "the exchange must be bounded, took {elapsed:?}"
        );
        held.abort();
    }

    /// The same bound does not reject a socket with nothing listening at all —
    /// that path fails at `connect`, immediately.
    #[tokio::test]
    async fn an_absent_socket_is_not_a_running_daemon() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(!ping_within(&dir.path().join("absent.sock"), Duration::from_secs(2)).await);
    }

    fn environment(
        override_program: Option<&str>,
        path_entries: &[&str],
        known: &[&str],
        beside: Option<&str>,
    ) -> Environment {
        Environment {
            override_program: override_program.map(PathBuf::from),
            path_entries: path_entries.iter().map(PathBuf::from).collect(),
            known_directories: known.iter().map(PathBuf::from).collect(),
            beside_app: beside.map(PathBuf::from),
            packaged_runtime: None,
            development_build: None,
        }
    }

    fn present(paths: &[&str]) -> impl Fn(&Path) -> bool {
        let set: HashSet<PathBuf> = paths.iter().map(PathBuf::from).collect();
        move |path: &Path| set.contains(path)
    }

    #[test]
    fn the_single_binary_on_path_is_launched_as_its_own_daemon() {
        let env = environment(None, &["/usr/bin", "/usr/local/bin"], &[], None);
        let (invocation, source) =
            resolve_in(&env, &present(&["/usr/local/bin/codypendent"])).expect("found");
        assert_eq!(source, InvocationSource::Path);
        assert_eq!(invocation.program, "/usr/local/bin/codypendent");
        assert_eq!(invocation.args, vec!["__daemon".to_owned()]);
    }

    #[test]
    fn a_standalone_daemon_binary_takes_no_arguments() {
        let env = environment(None, &["/usr/local/bin"], &[], None);
        let (invocation, _) =
            resolve_in(&env, &present(&["/usr/local/bin/codypendentd"])).expect("found");
        assert_eq!(invocation.program, "/usr/local/bin/codypendentd");
        assert!(invocation.args.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_packaged_runtime_uses_the_tauri_product_resource_name() {
        let directory = tempfile::tempdir().unwrap();
        let binaries = directory.path().join("usr/bin");
        std::fs::create_dir_all(&binaries).unwrap();
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let product = config["productName"].as_str().unwrap();
        let resources = directory.path().join("usr/lib").join(product);
        assert_eq!(
            packaged_runtime_directory(&binaries),
            Some(resources.join("runtime"))
        );
        assert!(packaged_runtime_directory(&directory.path().join("target/debug")).is_none());
        std::fs::create_dir_all(&resources).unwrap();
        assert_eq!(
            packaged_runtime_directory(&binaries)
                .unwrap()
                .canonicalize()
                .ok(),
            None,
            "an absent runtime remains a package-owned missing target"
        );
        std::fs::create_dir_all(resources.join("runtime")).unwrap();
        assert_eq!(
            packaged_runtime_directory(&binaries)
                .unwrap()
                .canonicalize()
                .unwrap(),
            resources.join("runtime").canonicalize().unwrap()
        );
    }

    #[test]
    fn packaged_resource_runtime_needs_no_path_and_preserves_explicit_override() {
        let mut env = environment(None, &[], &[], Some("/app/usr/bin"));
        env.packaged_runtime = Some(PathBuf::from("/app/usr/lib/Codypendent/runtime"));
        let (invocation, source) = resolve_in(
            &env,
            &present(&["/app/usr/lib/Codypendent/runtime/codypendent"]),
        )
        .unwrap();
        assert_eq!(source, InvocationSource::BesideThisApp);
        assert_eq!(
            invocation.program,
            "/app/usr/lib/Codypendent/runtime/codypendent"
        );
        assert_eq!(invocation.args, vec!["__daemon".to_owned()]);
        env.override_program = Some(PathBuf::from("/explicit/codypendent"));
        let (invocation, source) = resolve_in(&env, &present(&["/explicit/codypendent"])).unwrap();
        assert_eq!(source, InvocationSource::Override);
        assert_eq!(invocation.program, "/explicit/codypendent");
    }

    #[test]
    fn missing_packaged_runtime_cannot_fall_back_to_an_installed_cli() {
        let mut env = environment(None, &["/usr/bin"], &[], Some("/app/usr/bin"));
        env.packaged_runtime = Some(PathBuf::from("/app/usr/lib/Codypendent/runtime"));
        env.development_build = Some(PathBuf::from("/workspace/target/debug"));
        let searched = resolve_in(
            &env,
            &present(&[
                "/usr/bin/codypendent",
                "/workspace/target/debug/codypendent",
                "/app/usr/bin/codypendent",
            ]),
        )
        .unwrap_err();
        assert_eq!(
            searched,
            vec!["/app/usr/lib/Codypendent/runtime/codypendent".to_owned()]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_whole_product_resource_tree_cannot_fall_back_to_path() {
        let directory = tempfile::tempdir().unwrap();
        let binaries = directory.path().join("usr/bin");
        std::fs::create_dir_all(&binaries).unwrap();
        let mut env = environment(None, &["/usr/bin"], &[], None);
        env.beside_app = Some(binaries.clone());
        env.packaged_runtime = packaged_runtime_directory(&binaries);
        let expected = env.packaged_runtime.as_ref().unwrap().join("codypendent");
        assert!(
            !expected.exists(),
            "the entire product resource tree is absent"
        );
        let searched = resolve_in(&env, &present(&["/usr/bin/codypendent"])).unwrap_err();
        assert_eq!(searched, vec![expected.display().to_string()]);
    }

    #[test]
    fn the_override_wins_and_a_bad_override_is_reported_not_trusted() {
        let env = environment(Some("/opt/dev/codypendent"), &["/usr/local/bin"], &[], None);
        let (invocation, source) = resolve_in(
            &env,
            &present(&["/opt/dev/codypendent", "/usr/local/bin/codypendent"]),
        )
        .expect("found");
        assert_eq!(source, InvocationSource::Override);
        assert_eq!(invocation.program, "/opt/dev/codypendent");

        let searched = resolve_in(&env, &present(&[])).expect_err("nothing there");
        assert_eq!(searched[0], "CODYPENDENT_BINARY=/opt/dev/codypendent");
        assert!(searched.contains(&"/usr/local/bin/codypendent".to_owned()));
    }

    #[test]
    fn bundled_daemon_precedes_path_and_known_installations() {
        let env = environment(
            None,
            &["/usr/bin"],
            &["/home/dan/.cargo/bin"],
            Some("/Applications/Codypendent.app/Contents/MacOS"),
        );
        let (invocation, source) =
            resolve_in(&env, &present(&["/home/dan/.cargo/bin/codypendent"])).expect("found");
        assert_eq!(source, InvocationSource::KnownDirectory);
        assert!(invocation.program.ends_with("/.cargo/bin/codypendent"));

        let (invocation, source) = resolve_in(
            &env,
            &present(&[
                "/Applications/Codypendent.app/Contents/MacOS/codypendentd",
                "/usr/bin/codypendent",
                "/home/dan/.cargo/bin/codypendent",
            ]),
        )
        .expect("found");
        assert_eq!(source, InvocationSource::BesideThisApp);
        assert!(invocation.args.is_empty());
    }

    #[test]
    fn development_daemon_precedes_installed_cli_but_not_bundle_or_override() {
        let mut env = environment(None, &["/usr/local/bin"], &[], Some("/app/Contents/MacOS"));
        env.development_build = Some(PathBuf::from("/workspace/target/debug"));
        let binaries = present(&[
            "/workspace/target/debug/codypendent",
            "/usr/local/bin/codypendent",
        ]);
        let (invocation, source) = resolve_in(&env, &binaries).expect("development daemon");
        assert_eq!(source, InvocationSource::DevelopmentBuild);
        assert_eq!(invocation.program, "/workspace/target/debug/codypendent");

        let (invocation, source) = resolve_in(
            &env,
            &present(&[
                "/app/Contents/MacOS/codypendent",
                "/workspace/target/debug/codypendent",
            ]),
        )
        .expect("bundled daemon");
        assert_eq!(source, InvocationSource::BesideThisApp);
        assert_eq!(invocation.program, "/app/Contents/MacOS/codypendent");

        env.override_program = Some(PathBuf::from("/usr/local/bin/codypendent"));
        let (_, source) = resolve_in(&env, &binaries).expect("explicit daemon");
        assert_eq!(source, InvocationSource::Override);
    }

    #[tokio::test]
    async fn an_answering_daemon_is_reported_as_already_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().join("data"));
        paths.ensure_directories().expect("dirs");
        let listener = tokio::net::UnixListener::bind(&paths.socket_path).expect("bind");
        // Answers every Ping until aborted: the status read below pings again.
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let request = read_envelope(&mut stream)
                    .await
                    .expect("read")
                    .expect("envelope");
                assert!(matches!(request.payload, Payload::Ping));
                let reply = Envelope::reply_to(&request, Payload::Pong);
                write_envelope(&mut stream, &reply).await.expect("write");
            }
        });
        assert!(ping(&paths.socket_path).await);
        let status = launch_status(&paths).await;
        assert!(status.listening);
        assert_eq!(status.manual_command, MANUAL_COMMAND);
        assert!(matches!(
            start_daemon(&paths).await.expect("already running"),
            StartOutcome::AlreadyRunning
        ));
        server.abort();
    }

    #[tokio::test]
    async fn a_silent_socket_is_not_listening() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().join("data"));
        assert!(!ping(&paths.socket_path).await);
        let status = launch_status(&paths).await;
        assert!(!status.listening);
        assert!(status.log_path.ends_with("daemon.log"));
    }
}
