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
//!
//! # `--policy` routing (Phase 7's "routing⇄eval composition" follow-up)
//!
//! When `eval run` is given `--policy NAME`, [`route_cases`] resolves EVERY
//! case's model through the real `codypendent-routing` engine — the same
//! `Router`, the same classification hard filter, and the same persisted
//! `model_profiles` the daemon's own routing seam
//! (`codypendentd::routing::RoutingCoordinator`) reads — fail-closed: an
//! unrecognized policy name, an empty profile store, or a case the router
//! refuses to route all stop `eval run` BEFORE any case executes, with a
//! clear, non-zero exit (never a silent fallback to the default model for a
//! policy that was explicitly requested). The resolved model is additively
//! recorded per case in the report ([`report_json_with_routing`]'s
//! `routed_model` field).
//!
//! **What this does not (yet) do:** pin the daemon's `StartRun` execution to
//! the routed model. `StartRun` (Chapter 03's wire command) carries no model
//! field — the daemon resolves its own model for every run (Phase-1
//! `resolve_model` over `models.toml`, or its own independently-configured
//! `<data_dir>/routing.toml` seam), with no per-command override; adding one
//! is a `codypendent-protocol`/`codypendent-daemon`/`codypendentd` change,
//! out of scope for this CLI-only task (named explicitly in the task report).
//! So `--policy` today proves out and records a real, fail-closed routing
//! decision per case; it does not yet change which model the case's own run
//! calls.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context;
use codypendent_daemon::db::open_database;
use codypendent_daemon::model_profiles::ModelProfileStore;
use codypendent_eval::{Assertion, EvalCase, RunObservation, SuiteReport};
use codypendent_protocol::{
    AgentMode, ApprovalDecision, ApprovalId, BudgetDimension, ClientRole, CommandBody,
    DataClassification, EventBody, ModelId, Payload, ProposedAction, RunId, RunState, Subscription,
    WorkspaceId,
};
use codypendent_routing::{
    classify, ModelProfile, RequiredCapabilities, Router, RoutingPolicy, TaskNode, TaskSignals,
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

// --- Phase 7 routing⇄eval composition: `--policy` --------------------------

/// The routing policies `eval run --policy` recognizes by name today. A full
/// named-policy registry (e.g. sourced from a `routing-policies/` directory)
/// is future work — see the roadmap's "routing⇄eval composition" note; for
/// now this is [`RoutingPolicy::balanced`], the only named policy that exists
/// anywhere in this codebase yet.
const KNOWN_POLICIES: &[&str] = &["balanced"];

/// Resolve `--policy NAME` to a [`RoutingPolicy`]. An unrecognized name is a
/// hard error naming every policy that IS known — never a silent default —
/// because a `--policy` that fails to resolve must stop `eval run`, not
/// quietly route every case onto the daemon's default model.
fn resolve_named_policy(name: &str) -> anyhow::Result<RoutingPolicy> {
    match name {
        "balanced" => Ok(RoutingPolicy::balanced()),
        other => anyhow::bail!(
            "eval: unknown routing policy `{other}` (known policies: {}); refusing rather than \
             silently falling back to the default model",
            KNOWN_POLICIES.join(", ")
        ),
    }
}

/// Route every case in `cases` to a model under the named `policy`, over the
/// model profiles persisted at `<data_dir>/codypendent.db` — the same
/// `model_profiles` store `codypendent models bench` writes and the daemon's
/// own routing seam (`codypendentd::routing::RoutingCoordinator`) reads. The
/// eval harness is a client, not the daemon, so there is no
/// `RoutingCoordinator` to reuse directly; this consults
/// [`codypendent_routing::Router`] itself, over the same persisted profiles
/// and the same classification hard filter (see [`eval_task_node`]).
///
/// **Fails closed, before any case runs:** an unrecognized policy name, an
/// empty profile store, or any single case the router refuses to route
/// (`RoutingError::NoEligibleModel` — e.g. the hard filter excludes every
/// stored model) all stop this with a clear error — `--policy` was
/// explicitly requested, so a misconfiguration is never masked by silently
/// falling back to the default model for some or all cases.
///
/// Returns the resolved `(case_id, ModelId)` pairs, in case order — recorded
/// into the report by [`report_json_with_routing`]. This selects and
/// validates the model; it does not (yet) change which model the case's
/// `StartRun` actually executes on — see this file's module doc.
pub async fn route_cases(
    paths: &codypendent_protocol::discovery::RuntimePaths,
    cases: &[EvalCase],
    policy_name: &str,
) -> anyhow::Result<Vec<(String, ModelId)>> {
    let policy = resolve_named_policy(policy_name)?;

    let db_path = paths.data_dir.join("codypendent.db");
    let pool = open_database(&db_path).await.with_context(|| {
        format!(
            "opening {} to read persisted model profiles",
            db_path.display()
        )
    })?;
    let stored = ModelProfileStore::new()
        .list(&pool)
        .await
        .context("loading persisted model profiles")?;
    if stored.is_empty() {
        anyhow::bail!(
            "eval: --policy {policy_name} requires measured model profiles, but none are \
             persisted at {} — run `codypendent models bench <id>` first; refusing rather than \
             silently falling back to the default model",
            db_path.display()
        );
    }
    let profiles: Vec<ModelProfile> = stored.into_iter().map(|entry| entry.profile).collect();
    let router = Router::new(&profiles, &policy);

    let mut decisions = Vec::with_capacity(cases.len());
    for case in cases {
        let node = eval_task_node(case);
        match router.route(&node) {
            Ok(decision) => decisions.push((case.id.clone(), decision.model)),
            Err(error) => anyhow::bail!(
                "eval: --policy {policy_name} could not route case `{}`: {error}; refusing \
                 rather than silently falling back to the default model",
                case.id
            ),
        }
    }
    Ok(decisions)
}

/// The [`TaskNode`] case `case` routes under: mode `build` (mirrors the
/// `AgentMode::Build` every [`run_case_over_connection`] call starts with),
/// node kind `"eval"`, the case's prompt as the objective, and — since
/// `EvalCase` carries no per-case [`DataClassification`] — a fail-closed
/// [`DataClassification::Unknown`] ceiling, exactly mirroring
/// `codypendentd::routing::RoutingCoordinator::build_task_node`'s own
/// fail-closed default: an eval case is never treated as low-sensitivity by
/// default, so a policy that only allows local models off-device still
/// routes eval cases to a local model rather than accidentally admitting a
/// hosted one.
fn eval_task_node(case: &EvalCase) -> TaskNode {
    let estimated_input_tokens = ((case.prompt.len() as u64) / 4).max(256);
    let classification = classify(&TaskSignals::from_objective(
        "build",
        "eval",
        estimated_input_tokens,
        &case.prompt,
    ));
    TaskNode {
        classification,
        required: RequiredCapabilities {
            tools: true,
            structured_output: true,
            ..Default::default()
        },
        data_classification: DataClassification::Unknown,
        estimated_input_tokens,
        estimated_output_tokens: 4_000,
    }
}

/// Serialize `report` to pretty JSON, additively merging `routed` (case id →
/// routed [`ModelId`], from [`route_cases`]) into each matching case object
/// as a new `routed_model` string field. [`SuiteReport`]/`CaseResult`'s own
/// Rust shape is untouched — neither type gains a field, so every existing
/// reader keeps working unmodified — only the JSON FILE gains an extra
/// per-case key, which a reader that does not know it simply ignores
/// (neither type derives `#[serde(deny_unknown_fields)]`). `routed` is
/// `None` when `--policy` was not given, in which case the output is
/// byte-for-byte identical to a plain `serde_json::to_string_pretty(report)`
/// — the untouched `eval-smoke` CI path.
pub fn report_json_with_routing(
    report: &SuiteReport,
    routed: Option<&[(String, ModelId)]>,
) -> anyhow::Result<String> {
    let Some(routed) = routed else {
        return Ok(serde_json::to_string_pretty(report)?);
    };
    let by_case: HashMap<&str, String> = routed
        .iter()
        .map(|(case_id, model)| (case_id.as_str(), model.to_string()))
        .collect();

    let mut value = serde_json::to_value(report).context("serializing the suite report")?;
    if let Some(results) = value.get_mut("results").and_then(|v| v.as_array_mut()) {
        for case_value in results {
            let case_id = case_value
                .get("case_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let model = case_id.as_deref().and_then(|id| by_case.get(id)).cloned();
            let Some(model) = model else { continue };
            if let Some(obj) = case_value.as_object_mut() {
                obj.insert("routed_model".to_string(), serde_json::Value::String(model));
            }
        }
    }
    Ok(serde_json::to_string_pretty(&value)?)
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
            // The eval harness pins no model; the daemon resolves/routes as usual.
            model: None,
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

#[cfg(test)]
mod policy_routing_tests {
    use super::*;
    use codypendent_routing::{
        ModelCapabilities, ModelExecutionProfile, ModelLocation, ModelPerformance,
        StructuredOutputSupport, ToolCallSupport,
    };
    use std::collections::BTreeMap;

    fn paths_over(dir: &std::path::Path) -> codypendent_protocol::discovery::RuntimePaths {
        let paths = codypendent_protocol::discovery::RuntimePaths::from_data_dir(dir.to_path_buf());
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        paths
    }

    fn caps() -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tools: ToolCallSupport::Parallel,
            parallel_tools: true,
            structured_output: StructuredOutputSupport::Strict,
            vision: false,
            audio_input: false,
            embeddings: false,
            prompt_caching: false,
            reasoning_controls: false,
            context_tokens: Some(200_000),
            output_tokens: Some(16_000),
        }
    }

    fn profile(id: &str, location: ModelLocation, reliability: f64) -> ModelProfile {
        ModelProfile {
            id: ModelId(id.to_string()),
            location,
            capabilities: caps(),
            performance: ModelPerformance {
                reliability,
                cost_per_1k_tokens_usd: 0.01,
                latency_ms_p50: 500.0,
                task_class_success: BTreeMap::new(),
                failure_patterns: vec![],
            },
            execution: ModelExecutionProfile::default(),
            bench: None,
        }
    }

    fn one_case() -> EvalCase {
        EvalCase {
            id: "case-a".to_string(),
            repository_revision: "0".repeat(40),
            prompt: "fix the failing test in paginate".to_string(),
            policy: "coding-balanced".to_string(),
            expected: vec![],
            maximum_cost_usd: None,
            maximum_duration_ms: None,
            task_class: None,
        }
    }

    #[tokio::test]
    async fn route_cases_selects_the_eligible_local_model_under_balanced() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_over(dir.path());
        let pool = open_database(&paths.data_dir.join("codypendent.db"))
            .await
            .unwrap();
        ModelProfileStore::new()
            .upsert(
                &pool,
                "http://localhost:11434/v1",
                &profile("local-coder", ModelLocation::Local, 0.9),
            )
            .await
            .unwrap();

        let cases = vec![one_case()];
        let routed = route_cases(&paths, &cases, "balanced").await.unwrap();
        assert_eq!(
            routed,
            vec![("case-a".to_string(), ModelId("local-coder".to_string()))]
        );
    }

    #[tokio::test]
    async fn route_cases_fails_closed_when_only_a_hosted_model_is_stored() {
        // An eval case's classification is fail-closed `Unknown` (no per-case
        // classification exists on `EvalCase`); `balanced`'s off-device
        // ceiling is `Confidential`, strictly less sensitive than `Unknown`,
        // so a hosted model is never eligible — proving the classification
        // hard filter actually runs here, not just documented as intent.
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_over(dir.path());
        let pool = open_database(&paths.data_dir.join("codypendent.db"))
            .await
            .unwrap();
        ModelProfileStore::new()
            .upsert(
                &pool,
                "https://api.example.com/v1",
                &profile("hosted-model", ModelLocation::Hosted, 0.99),
            )
            .await
            .unwrap();

        let cases = vec![one_case()];
        let error = route_cases(&paths, &cases, "balanced").await.unwrap_err();
        assert!(
            error.to_string().contains("could not route case"),
            "error: {error}"
        );
    }

    #[tokio::test]
    async fn route_cases_fails_closed_when_no_profiles_are_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_over(dir.path());
        // Ensure the DB (and its empty `model_profiles` table) exists, but
        // nothing is stored in it.
        open_database(&paths.data_dir.join("codypendent.db"))
            .await
            .unwrap();

        let cases = vec![one_case()];
        let error = route_cases(&paths, &cases, "balanced").await.unwrap_err();
        assert!(
            error.to_string().contains("models bench"),
            "error should point at the fix: {error}"
        );
    }

    #[tokio::test]
    async fn route_cases_fails_closed_on_an_unrecognized_policy_name() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_over(dir.path());
        let cases = vec![one_case()];
        let error = route_cases(&paths, &cases, "nonexistent")
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("unknown routing policy"),
            "error: {error}"
        );
    }

    #[test]
    fn report_json_with_routing_is_unchanged_when_no_policy_was_used() {
        let report = SuiteReport::new(vec![]);
        let plain = serde_json::to_string_pretty(&report).unwrap();
        let via_helper = report_json_with_routing(&report, None).unwrap();
        assert_eq!(plain, via_helper);
    }

    #[test]
    fn report_json_with_routing_additively_merges_the_routed_model() {
        let case_result = codypendent_eval::CaseResult {
            case_id: "case-a".to_string(),
            assertion_results: vec![],
            within_cost: true,
            within_duration: true,
        };
        let report = SuiteReport::new(vec![case_result]);
        let routed = vec![("case-a".to_string(), ModelId("local-coder".to_string()))];

        let json = report_json_with_routing(&report, Some(&routed)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["results"][0]["routed_model"], "local-coder");
        // The original shape still round-trips through `SuiteReport` (the
        // extra key is additive, never breaking an existing reader).
        let round_tripped: SuiteReport = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped, report);
    }
}
