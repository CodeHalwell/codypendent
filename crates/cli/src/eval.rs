//! `codypendent eval run` (Phase 7 STEP 7.1): drive an `evals/tasks/` suite
//! headlessly over the JSONL client, score each case's assertions against an
//! objective [`RunObservation`], and write a [`SuiteReport`].
//!
//! # Building the observation
//!
//! [`RunObservation`]'s fields come from two places, deliberately kept
//! separate because the wire protocol never inlines bulk content (Chapter 03):
//!
//! * **The event stream** (`approval_requested`, `executed_commands`,
//!   `network_hosts`, `cost_usd`): [`ObservationBuilder`] reconstructs these
//!   from `ApprovalRequested`/`ApprovalResolved`/`BudgetWarning` events as the
//!   run streams by. Only an **approved** `ProposedAction` counts as executed
//!   or contacted — a rejected proposal never ran. This means an action that
//!   somehow executes *without* going through the approval flow is invisible
//!   to this builder; every `ExecuteCommand`/`NetworkRequest` this codebase's
//!   policy engine reaches is approval-gated (Chapter 03's "approval-gated
//!   writes" invariant), so this is a narrow, documented gap rather than a
//!   silent one (see the task report for the exact scope).
//! * **The checked-out working tree** (`changed_files`, `existing_symbols`,
//!   `tests_passed`): [`inspect_repository`] shells out to `git`/the fixture's
//!   own test command *after* the run completes, comparing against the case's
//!   pinned `repository_revision`. This is the only way to answer "did file X
//!   change" or "do the tests pass" — those facts live in the repository, not
//!   in any event.
//!
//! `correct_citations` has no wire signal yet (no event carries a claim/source
//! pair) and is always empty; a `CitationCorrect` assertion in a case
//! therefore always fails today — out of scope for this task, named in the
//! report.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context;
use codypendent_eval::{Assertion, EvalCase, RunObservation, SuiteReport};
use codypendent_protocol::{
    AgentMode, ApprovalDecision, ApprovalId, BudgetDimension, ClientRole, CommandBody, EventBody,
    Payload, ProposedAction, RunId, RunState, Subscription, WorkspaceId,
};

use crate::commands::{ensure_daemon, expect_catchup};
use crate::connection::Connection;
use crate::stream::event_run_id;

/// Load every case JSON file directly under `suite_dir` (non-recursive,
/// sorted by filename for a deterministic order), parsing each into an
/// [`EvalCase`]. A file that fails to parse names itself in the error so a
/// broken fixture is easy to find.
pub fn load_suite(suite_dir: &Path) -> anyhow::Result<Vec<EvalCase>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(suite_dir)
        .with_context(|| format!("reading suite directory {}", suite_dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    paths.sort();
    if paths.is_empty() {
        anyhow::bail!(
            "no *.json case files found in {} — is this an eval suite directory?",
            suite_dir.display()
        );
    }
    paths
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading case file {}", path.display()))?;
            serde_json::from_str::<EvalCase>(&text)
                .with_context(|| format!("parsing case file {}", path.display()))
        })
        .collect()
}

/// Resolve `--suite NAME` to a directory: `evals/tasks/<name>` under the
/// current directory if that exists, otherwise `NAME` itself taken as a direct
/// path (so a suite outside the default layout, or an absolute path, also
/// works).
pub fn resolve_suite_dir(suite: &str) -> anyhow::Result<PathBuf> {
    let under_tasks = PathBuf::from("evals").join("tasks").join(suite);
    if under_tasks.is_dir() {
        return Ok(under_tasks);
    }
    let direct = PathBuf::from(suite);
    if direct.is_dir() {
        return Ok(direct);
    }
    anyhow::bail!(
        "no suite directory found at {} or {} (run from the repository root, \
         or pass a path with --suite)",
        under_tasks.display(),
        direct.display()
    )
}

/// The fixture repository a suite's cases run against:
/// `<suite_dir>/../../fixtures/<name>.bundle` (i.e. `evals/fixtures/<name>.bundle`
/// — sibling to `evals/tasks/`), by convention (see `evals/README.md`).
/// `EvalCase` carries only a `repository_revision`, not a repository path, so
/// the suite establishes which vendored fixture its cases share.
///
/// The fixture is vendored as a `git bundle` — a single file capturing the
/// fixture's full history — rather than a plain checkout, because a plain
/// checkout would need its own nested `.git` directory, which `git` (in the
/// PARENT repository, this one) treats as a submodule gitlink rather than
/// tracked file content. A bundle is an ordinary blob to the parent repo and
/// `git clone` accepts it directly as a clone source (verified in this task's
/// report), so [`checkout_fixture`] clones from it exactly as it would from a
/// live remote.
pub fn fixture_root(suite_dir: &Path, fixture_name: &str) -> anyhow::Result<PathBuf> {
    let evals_root = suite_dir
        .parent() // evals/tasks
        .and_then(Path::parent) // evals
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cannot locate the evals/ root above suite directory {}",
                suite_dir.display()
            )
        })?;
    let root = evals_root
        .join("fixtures")
        .join(format!("{fixture_name}.bundle"));
    if !root.is_file() {
        anyhow::bail!(
            "fixture bundle not found at {} (referenced by the suite's cases)",
            root.display()
        );
    }
    Ok(root)
}

/// Run every case in `cases` against fixture `fixture_root`, headlessly,
/// returning the aggregate [`SuiteReport`]. Ensures a daemon once up front;
/// each case gets its own fresh session/run/scratch checkout so cases never
/// interfere with each other.
pub async fn run_suite(
    paths: &codypendent_protocol::discovery::RuntimePaths,
    cases: &[EvalCase],
    fixture_root: &Path,
) -> anyhow::Result<SuiteReport> {
    ensure_daemon(paths).await?;
    let mut results = Vec::with_capacity(cases.len());
    for case in cases {
        eprintln!("eval: running {}", case.id);
        let (_scratch, checkout) = checkout_fixture(fixture_root, &case.repository_revision)
            .await
            .with_context(|| format!("preparing the fixture checkout for case {}", case.id))?;

        let mut conn = Connection::connect(&paths.socket_path).await?;
        let result = run_case(&mut conn, case, &checkout).await?;
        eprintln!(
            "eval: {} {}",
            case.id,
            if result.passed() { "PASS" } else { "FAIL" }
        );
        results.push(result);
    }
    Ok(SuiteReport::new(results))
}

/// Run one case to a [`CaseResult`]: drive it headlessly over `conn` (already
/// connected; this handshakes it), then fill in the repository-derived facts
/// from `checkout`, then score. Split out from [`run_suite`]'s loop so a test
/// can drive exactly this pipeline — wire observation AND repository
/// inspection — against a hand-rolled mock daemon and a real (but tiny,
/// throwaway) git checkout, without a live daemon or model.
pub async fn run_case(
    conn: &mut Connection,
    case: &EvalCase,
    checkout: &Path,
) -> anyhow::Result<codypendent_eval::CaseResult> {
    let mut obs = run_case_over_connection(conn, case, checkout).await?;
    inspect_repository(checkout, case, &mut obs).await?;
    Ok(case.score(&obs))
}

/// The connected core of one case's headless run: handshake, create a session,
/// attach as `Controller`, start the run, and stream events until it reaches a
/// terminal state — building a [`RunObservation`] from the stream as it goes.
/// Split out (like `commands::run_over_connection`) so a test can drive it
/// against a hand-rolled mock daemon instead of a live one.
pub async fn run_case_over_connection(
    conn: &mut Connection,
    case: &EvalCase,
    repository: &Path,
) -> anyhow::Result<RunObservation> {
    conn.handshake("codypendent-eval", env!("CARGO_PKG_VERSION"), None)
        .await?;

    let workspace = WorkspaceId::new();
    let create_reply = conn
        .send_command(CommandBody::CreateSession {
            workspace,
            title: format!("eval: {}", case.id),
        })
        .await?;
    let session_id = match &create_reply.payload {
        Payload::CommandAccepted { .. } => create_reply.session_id.ok_or_else(|| {
            anyhow::anyhow!("daemon accepted CreateSession but its reply carried no session_id")
        })?,
        Payload::CommandRejected(error) => {
            anyhow::bail!("CreateSession rejected: {} ({})", error.message, error.code)
        }
        other => anyhow::bail!("unexpected reply to CreateSession: {other:?}"),
    };

    let attach_reply = conn
        .send_command(CommandBody::AttachSession {
            session_id,
            last_seen_sequence: None,
            subscriptions: vec![Subscription::SessionSummary, Subscription::AgentActivity],
            requested_role: ClientRole::Controller,
        })
        .await?;
    // A freshly created session has nothing to catch up on; the reply is
    // still validated (a rejection here would mean something is badly wrong)
    // but its content is discarded — `eval run`'s output is the SuiteReport,
    // never a JSONL transcript.
    expect_catchup(attach_reply)?;

    let started = Instant::now();
    let start_reply = conn
        .send_command(CommandBody::StartRun {
            session_id,
            objective: case.prompt.clone(),
            mode: AgentMode::Build,
            repository: Some(repository.to_string_lossy().into_owned()),
        })
        .await?;
    if let Payload::CommandRejected(error) = &start_reply.payload {
        anyhow::bail!("StartRun rejected: {} ({})", error.message, error.code);
    }
    let mut run_id: Option<RunId> = match &start_reply.payload {
        Payload::CommandAccepted { created_run, .. } => *created_run,
        _ => None,
    };

    let mut builder = ObservationBuilder::default();
    loop {
        let envelope = conn.next_envelope().await?.ok_or_else(|| {
            anyhow::anyhow!("daemon closed the connection before the run reached a terminal state")
        })?;
        let Payload::Event(event) = &envelope.payload else {
            continue;
        };
        if let EventBody::RunStarted { run_id: rid, .. } = &event.body {
            run_id.get_or_insert(*rid);
        }
        // This session was just created exclusively for this one case, so
        // (unlike `stream::stream_until_terminal`, which must disambiguate a
        // session shared by concurrent runs) EVERY event observed here belongs
        // to it — including `ApprovalRequested`/`ApprovalResolved`, which carry
        // no `run_id` field at all and so could never pass a per-run
        // ownership filter. Every event is folded into the observation; the
        // run-ownership check below is used only to decide when to STOP
        // reading (a run-scoped event whose id doesn't match ours — which
        // should not happen in a session this runner owns exclusively, but
        // mirrors the same defensive check `stream_until_terminal` makes).
        builder.observe(&event.body);
        let owns_event = matches!(event_run_id(&event.body), Some(rid) if Some(rid) == run_id);
        if owns_event && is_terminal(&event.body) {
            break;
        }
    }
    builder.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(builder.finish())
}

/// Whether `body` is the terminal event of the run it belongs to (mirrors
/// `commands::run_over_connection`'s two documented terminal signals).
fn is_terminal(body: &EventBody) -> bool {
    matches!(body, EventBody::RunCompleted { .. })
        || matches!(
            body,
            EventBody::RunStateChanged {
                state: RunState::Completed | RunState::Failed | RunState::Cancelled,
                ..
            }
        )
}

/// Accumulates the wire-observable half of a [`RunObservation`] while a case's
/// events stream by. The repository-observable half (`changed_files`,
/// `existing_symbols`, `tests_passed`) is filled in afterward by
/// [`inspect_repository`].
#[derive(Debug, Default)]
struct ObservationBuilder {
    approval_requested: bool,
    executed_commands: Vec<String>,
    network_hosts: Vec<String>,
    cost_usd: f64,
    duration_ms: u64,
    /// A proposed action's approval is requested, then resolved, as two
    /// separate events correlated by `approval_id`; only a *resolved-approve*
    /// counts as executed/contacted, so the action is held here in between.
    pending: HashMap<ApprovalId, ProposedAction>,
}

impl ObservationBuilder {
    fn observe(&mut self, body: &EventBody) {
        match body {
            EventBody::ApprovalRequested {
                approval_id,
                action,
                ..
            } => {
                self.approval_requested = true;
                self.pending.insert(*approval_id, action.clone());
            }
            EventBody::ApprovalResolved {
                approval_id,
                decision,
            } => {
                if let Some(action) = self.pending.remove(approval_id) {
                    if matches!(decision, ApprovalDecision::Approve) {
                        self.record_approved(&action);
                    }
                }
            }
            EventBody::BudgetWarning {
                dimension: BudgetDimension::Cost,
                used,
                ..
            } => {
                // `used` is in minor currency units (cents); the latest warning
                // is the most current running total.
                self.cost_usd = *used as f64 / 100.0;
            }
            _ => {}
        }
    }

    /// Record the observable effect of an action that was actually approved —
    /// a rejected proposal never ran, so it must never appear here.
    fn record_approved(&mut self, action: &ProposedAction) {
        match action {
            ProposedAction::ExecuteCommand { program, args, .. } => {
                let mut line = program.clone();
                for arg in args {
                    line.push(' ');
                    line.push_str(arg);
                }
                self.executed_commands.push(line);
            }
            ProposedAction::NetworkRequest { destination } => {
                self.network_hosts.push(destination.clone());
            }
            ProposedAction::GitPush { remote, .. } => {
                self.network_hosts.push(remote.clone());
            }
            ProposedAction::GitHubMutation { .. } => {
                self.network_hosts.push("api.github.com".to_string());
            }
            // ReadFiles/WritePatch/GitCommit touch no network host and are not
            // "executed commands" in the shell sense; the repository inspection
            // pass (`inspect_repository`) is what proves a patch's effect.
            _ => {}
        }
    }

    fn finish(self) -> RunObservation {
        RunObservation {
            approval_requested: self.approval_requested,
            executed_commands: self.executed_commands,
            network_hosts: self.network_hosts,
            cost_usd: self.cost_usd,
            duration_ms: self.duration_ms,
            ..Default::default()
        }
    }
}

/// Fill in the repository-derived facts an event stream cannot answer:
/// `changed_files` (a working-tree diff against the pinned starting revision,
/// tracked and untracked), and — only when the case actually asserts on them
/// (skipped otherwise to avoid a needless `cargo test`/`git grep`) —
/// `existing_symbols` and `tests_passed`.
async fn inspect_repository(
    repository: &Path,
    case: &EvalCase,
    obs: &mut RunObservation,
) -> anyhow::Result<()> {
    obs.changed_files = git_changed_files(repository, &case.repository_revision).await?;
    obs.patch_files_changed = obs.changed_files.len();

    if case
        .expected
        .iter()
        .any(|a| matches!(a, Assertion::SymbolExists { .. }))
    {
        for assertion in &case.expected {
            if let Assertion::SymbolExists { symbol } = assertion {
                if git_grep_has_match(repository, symbol).await? {
                    obs.existing_symbols.push(symbol.clone());
                }
            }
        }
    }

    if case
        .expected
        .iter()
        .any(|a| matches!(a, Assertion::TestsPass))
    {
        obs.tests_passed = Some(run_fixture_tests(repository).await?);
    }

    Ok(())
}

/// Every path that differs from `base_revision` in `repository`'s working
/// tree: tracked changes (`git diff --name-only`) plus untracked new files
/// (`git ls-files --others --exclude-standard`), deduplicated.
async fn git_changed_files(repository: &Path, base_revision: &str) -> anyhow::Result<Vec<String>> {
    let diffed = run_git(repository, &["diff", "--name-only", base_revision])
        .await
        .with_context(|| "diffing the working tree against the pinned revision")?;
    let untracked = run_git(repository, &["ls-files", "--others", "--exclude-standard"])
        .await
        .with_context(|| "listing untracked files")?;
    let mut files: Vec<String> = diffed
        .lines()
        .chain(untracked.lines())
        .map(str::to_string)
        .filter(|line| !line.is_empty())
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

/// Whether `symbol` appears literally anywhere in the repository's tracked
/// files (`git grep`, which respects `.gitignore` and searches only tracked
/// content) — a simple, honest proxy for "this symbol exists"; it does not
/// parse the language, so a comment or string containing the same text also
/// matches (a known, documented imprecision, not a fabrication).
async fn git_grep_has_match(repository: &Path, symbol: &str) -> anyhow::Result<bool> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(repository)
        .args(["grep", "--quiet", "-e", symbol]);
    let status = command
        .status()
        .await
        .with_context(|| format!("running git grep for {symbol:?}"))?;
    // `git grep` exits 0 on a match, 1 on no match, >1 on a real error.
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => anyhow::bail!("git grep for {symbol:?} exited abnormally: {status}"),
    }
}

/// Run the fixture's test suite and report whether it passed. Only `cargo
/// test` is recognized today (the vendored core-suite fixture is a Rust
/// crate); a fixture with no recognized test command reports `false` rather
/// than silently skipping — a case that asserts `TestsPass` against a fixture
/// this runner cannot test has not demonstrated a pass.
async fn run_fixture_tests(repository: &Path) -> anyhow::Result<bool> {
    if !repository.join("Cargo.toml").is_file() {
        return Ok(false);
    }
    // Captured (not inherited) so a fixture's own test output never bleeds
    // into `eval run`'s stdout/stderr or a CI log for the harness itself.
    let output = tokio::process::Command::new("cargo")
        .arg("test")
        .arg("--quiet")
        .current_dir(repository)
        .output()
        .await
        .with_context(|| "running cargo test in the fixture checkout")?;
    Ok(output.status.success())
}

/// Clone `fixture_bundle` (a `git bundle` file — see [`fixture_root`]) into a
/// fresh scratch directory and check out `revision`, verifying HEAD actually
/// landed there (a pin that silently drifted — a moved branch, a shallow
/// clone missing the commit — would make every downstream assertion
/// meaningless). `git clone` accepts a bundle file as its source exactly like
/// a live remote. Returns the owning `TempDir` (dropping it removes the
/// checkout) and the checkout's path.
async fn checkout_fixture(
    fixture_bundle: &Path,
    revision: &str,
) -> anyhow::Result<(tempfile::TempDir, PathBuf)> {
    let scratch = tempfile::tempdir().context("creating a scratch checkout directory")?;
    let dest = scratch.path().join("checkout");
    run_git(
        Path::new("."),
        &[
            "clone",
            "--quiet",
            &fixture_bundle.to_string_lossy(),
            &dest.to_string_lossy(),
        ],
    )
    .await
    .with_context(|| format!("cloning fixture bundle {}", fixture_bundle.display()))?;
    run_git(&dest, &["checkout", "--quiet", revision])
        .await
        .with_context(|| format!("checking out pinned revision {revision}"))?;
    let head = run_git(&dest, &["rev-parse", "HEAD"]).await?;
    if !head.trim().starts_with(revision) && head.trim() != revision {
        anyhow::bail!(
            "pinned revision drifted: expected {revision}, checkout resolved to {}",
            head.trim()
        );
    }
    Ok((scratch, dest))
}

/// Run `git <args>` in `cwd`, returning trimmed stdout on success or a
/// descriptive error including stderr on failure.
async fn run_git(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .await
        .with_context(|| format!("spawning git {args:?}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {args:?} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
