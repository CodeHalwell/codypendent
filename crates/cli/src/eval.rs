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
//!   `approved_commands`, `network_hosts`, `cost_usd`): [`ObservationBuilder`]
//!   reconstructs these from `ApprovalRequested`/`ApprovalResolved`/
//!   `ToolProposed`/`ToolCompleted`/`BudgetWarning` events as the run streams
//!   by. A rejected proposal never ran, so only an **approved**
//!   `ProposedAction` is recorded at all. This means an action that somehow
//!   executes *without* going through the approval flow is invisible to this
//!   builder; every `ExecuteCommand`/`NetworkRequest` this codebase's policy
//!   engine reaches is approval-gated (Chapter 03's "approval-gated writes"
//!   invariant), so this is a narrow, documented gap rather than a silent one
//!   (see the task report for the exact scope).
//!
//!   **Approval is not execution**, and the two directions therefore need two
//!   sets. An approved command lands in `approved_commands`; it reaches
//!   `executed_commands` only once [`ObservationBuilder::settle_completion`]
//!   correlates it with a `ToolCompleted { outcome: Succeeded }` for the same
//!   run. `command-executed` — the round's positive, evidence-of-work
//!   assertion — reads the confirmed set, so a command that failed to start or
//!   exited non-zero can no longer prove work was done; `command-not-executed`
//!   and `command-denied` read the over-broad attempted set, where
//!   over-reporting is the safe error. `network_hosts` stays the approved set
//!   for exactly that reason: both of its readers are negative (see
//!   [`RunObservation`]'s own field docs).
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
//! `routed_model` field) **and** pinned into that same case's own
//! `StartRun.model` (MP2's pin field — see [`run_suite`], [`run_case`],
//! [`run_case_over_connection`]): both read the SAME `(case_id, ModelId)`
//! pairs [`route_cases`] returned, so the model the report says ran is always
//! the model that actually ran — recorded == pinned == executed, one source
//! of truth. This closes the previously-deferred execution-pin gap: before,
//! every case sent `StartRun { model: None }` regardless of what the report
//! claimed, so the daemon could silently resolve a *different* model than the
//! one recorded, corrupting the experiment. When `--policy` is absent,
//! `routed` is `None` and every case still sends `model: None`, byte-for-byte
//! unchanged (the untouched `eval-smoke` CI path).
//!
//! **The pin never bypasses the daemon's own security filter.** The daemon
//! honors a pinned model through MP2's `RoutingCoordinator::validate_pin`,
//! which applies the identical classification hard filter
//! (`Router::passes_classification`) the router itself used to select a
//! model in the first place: a HOSTED model must clear the daemon's
//! off-device ceiling, but a LOCAL model clears it unconditionally,
//! regardless of classification or policy. [`eval_task_node`] deliberately
//! classifies every case at the most restrictive [`DataClassification::Unknown`]
//! (fail-closed, mirroring `RoutingCoordinator::build_task_node`'s own
//! default), and the only named policy `eval run --policy` supports today
//! (`balanced`, `max_off_device: Confidential`) does not admit `Unknown` data
//! off-device — so [`route_cases`] can only ever select a LOCAL model. A
//! local model's pin always clears `validate_pin`'s classification check
//! (the `model.is_local() || …` short-circuit), independent of the daemon's
//! own — possibly differently-configured — routing policy or classification
//! ceiling. So this pin can never turn a case that used to run into one the
//! daemon newly refuses. A future named policy whose `max_off_device` let
//! `route_cases` select a HOSTED model would need to re-examine this
//! invariant (see `route_cases_fails_closed_when_only_a_hosted_model_is_stored`
//! here and the `validate_pin_*` tests in `codypendentd::routing`).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context;
use codypendent_daemon::db::open_database;
use codypendent_daemon::model_profiles::ModelProfileStore;
use codypendent_eval::{Assertion, EvalCase, RunObservation, SuiteReport};
use codypendent_protocol::{
    AgentMode, ApprovalDecision, ApprovalId, ApprovalScope, BudgetDimension, ClientRole,
    CommandBody, DataClassification, EventBody, ModelId, Payload, ProposedAction, RunDisposition,
    RunId, RunState, Subscription, ToolOutcome, WorkspaceId,
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
/// into the report by [`report_json_with_routing`] AND pinned into each
/// case's own `StartRun.model` by [`run_suite`] (via [`routed_model_for_case`]),
/// so the model selected here is the SAME model recorded and the SAME model
/// executed — see this file's module doc for the classification-safety
/// argument that makes pinning it safe.
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

/// `case_id`'s routed model, looked up from the SAME `(case_id, ModelId)`
/// pairs [`route_cases`] returned and [`report_json_with_routing`] records
/// into the report — the one source of truth [`run_suite`] pins into that
/// case's `StartRun` (via [`run_case`]/[`run_case_over_connection`]), so the
/// model recorded in the report and the model actually executed can never
/// diverge. `None` when `routed` is `None` (no `--policy` was given — every
/// case then sends `model: None`, unchanged) or when `case_id` has no entry
/// (defensive; [`route_cases`] resolves every case or fails closed before any
/// case runs, so this should not happen in practice).
#[must_use]
fn routed_model_for_case(routed: Option<&[(String, ModelId)]>, case_id: &str) -> Option<ModelId> {
    routed?
        .iter()
        .find(|(id, _)| id == case_id)
        .map(|(_, model)| model.clone())
}

/// Run every case in `cases` against fixture `fixture_root`, headlessly,
/// returning the aggregate [`SuiteReport`]. Ensures a daemon once up front;
/// each case gets its own fresh session/run/scratch checkout so cases never
/// interfere with each other.
///
/// `routed` is the SAME `(case_id, ModelId)` pairs [`route_cases`] resolved
/// (and [`report_json_with_routing`] records) — `None` when `--policy` was
/// not given. Each case's routed model, if any, is pinned into its own
/// `StartRun.model` (see [`routed_model_for_case`]), so the model this run
/// executes on is always the model the report attributes to it.
///
/// **Also grades and clusters every case's [`codypendent_eval::Trace`]**
/// (STEP 7.4's `codypendent_eval::grade`/`cluster` — previously a
/// well-tested library with no production producer or consumer; see the
/// task report). This is additive to [`SuiteReport`] itself (which
/// `crate::commands::eval_run` — not owned by this module — reads and
/// writes unchanged) and reported ONLY over stderr, the same channel the
/// per-case `PASS`/`FAIL` lines already use, so no caller's parsing of the
/// returned [`SuiteReport`] or the written report file changes shape.
pub async fn run_suite(
    paths: &codypendent_protocol::discovery::RuntimePaths,
    cases: &[EvalCase],
    fixture_root: &Path,
    routed: Option<&[(String, ModelId)]>,
) -> anyhow::Result<SuiteReport> {
    ensure_daemon(paths).await?;
    let mut results = Vec::with_capacity(cases.len());
    let mut grades = Vec::with_capacity(cases.len());
    for case in cases {
        eprintln!("eval: running {}", case.id);
        let (_scratch, checkout) = checkout_fixture(fixture_root, &case.repository_revision)
            .await
            .with_context(|| format!("preparing the fixture checkout for case {}", case.id))?;

        let mut conn = Connection::connect(&paths.socket_path).await?;
        let routed_model = routed_model_for_case(routed, &case.id);
        let (result, trace) = run_case_with_trace(&mut conn, case, &checkout, routed_model).await?;
        eprintln!(
            "eval: {} {}",
            case.id,
            if result.passed() { "PASS" } else { "FAIL" }
        );
        grades.push(codypendent_eval::grade(&trace));
        results.push(result);
    }
    report_failure_clusters(&grades);
    Ok(SuiteReport::new(results))
}

/// Print a deterministic, most-frequent-first summary of this run's failure
/// clusters to stderr — [`codypendent_eval::cluster_failures`]'s one
/// production call site. Silent when nothing has a negative signal (an
/// all-passing suite prints nothing extra beyond the existing PASS lines).
fn report_failure_clusters(grades: &[codypendent_eval::TraceGrade]) {
    let failing: Vec<codypendent_eval::TraceGrade> = grades
        .iter()
        .filter(|g| g.has_negative_signal())
        .cloned()
        .collect();
    if failing.is_empty() {
        return;
    }
    let clusters = codypendent_eval::cluster_failures(&failing);
    let clusters = codypendent_eval::rank_by_frequency(clusters);
    eprintln!(
        "eval: {} failing trace(s) across {} failure cluster(s):",
        failing.len(),
        clusters.len()
    );
    for cluster in &clusters {
        eprintln!(
            "eval:   [{}x] {} / {} (tool: {}) — {}",
            cluster.count(),
            cluster.key.task_class,
            cluster.key.failing_signal.as_str(),
            cluster.key.tool.as_deref().unwrap_or("none"),
            cluster.exemplars.join(", ")
        );
    }
}

/// Run one case to a [`CaseResult`](codypendent_eval::CaseResult), discarding
/// the [`codypendent_eval::Trace`] built alongside it. Kept as the stable,
/// minimal-surface entry point `crates/cli/tests/eval_it.rs` and any
/// external caller drives directly against a hand-rolled mock daemon;
/// [`run_suite`] uses [`run_case_with_trace`] so it can grade and cluster
/// the suite's failures.
pub async fn run_case(
    conn: &mut Connection,
    case: &EvalCase,
    checkout: &Path,
    routed_model: Option<ModelId>,
) -> anyhow::Result<codypendent_eval::CaseResult> {
    let (result, _trace) = run_case_with_trace(conn, case, checkout, routed_model).await?;
    Ok(result)
}

/// Like [`run_case`], but also returns the [`codypendent_eval::Trace`] built
/// from the same observation — the STEP 7.4 wiring `codypendent_eval::grade`/
/// `cluster` previously had no production producer for. Built here, where
/// the full [`RunObservation`] is still in scope, rather than re-derived
/// later from a bare [`codypendent_eval::CaseResult`]: several `Trace`
/// fields (`tool`, `command_failures`) need facts (`executed_commands`,
/// per-assertion detail against `obs` directly) a bare
/// [`codypendent_eval::CaseResult`] — assertion pass/fail only — has
/// already discarded.
/// Drive it headlessly over `conn` (already connected; this handshakes it),
/// then fill in the repository-derived facts from `checkout`, then score.
/// Split out from [`run_suite`]'s loop so a test can drive exactly this
/// pipeline — wire observation AND repository inspection — against a
/// hand-rolled mock daemon and a real (but tiny, throwaway) git checkout,
/// without a live daemon or model. `routed_model` (when `Some`) is the model
/// this case is pinned to (see [`run_suite`]); `None` sends
/// `StartRun { model: None }`, unchanged.
pub async fn run_case_with_trace(
    conn: &mut Connection,
    case: &EvalCase,
    checkout: &Path,
    routed_model: Option<ModelId>,
) -> anyhow::Result<(codypendent_eval::CaseResult, codypendent_eval::Trace)> {
    let (mut obs, run_id) = run_case_over_connection(conn, case, checkout, routed_model).await?;
    inspect_repository(checkout, run_id, case, &mut obs).await?;
    let result = case.score(&obs);
    let trace = codypendent_eval::Trace::from_case(case, &result, &obs);
    Ok((result, trace))
}

/// Run every case in `cases` as a [`codypendent_eval::RegressionSuite`] guard
/// batch, returning the real [`codypendent_eval::RegressionReport`] —
/// [`codypendent_eval::RegressionSuite`]'s own `evaluate`, not an ad hoc
/// re-implementation of its "a case with no observation counts as
/// regressed" rule. Before this, nothing in the workspace ever built a
/// `RegressionSuite` from a real run; every prior use was a hand-built
/// `BTreeMap` in a unit test (see the task report). Mirrors [`run_suite`]
/// case for case (fresh daemon-ensure, one checkout per case, one connection
/// per case) but keeps every case's raw [`RunObservation`] — never
/// discarding it into a bare pass/fail — because that is exactly what
/// [`codypendent_eval::RegressionSuite::evaluate`] needs to tell "failed"
/// apart from "never observed".
///
/// Intended for `evals/tasks/regressions/` (see that directory's own
/// `README.md`): guard cases for historical failures that must never silently
/// come back. It sits under `evals/tasks/` — not at `evals/regressions/`, where
/// it used to live — because [`fixture_root`] resolves a suite's bundle at
/// `<suite_dir>/../../fixtures/`, so a suite one level shallower loaded its
/// cases and then died looking for `<repo>/fixtures/`.
/// No named CLI flag drives this yet — see `.impl/proposals/
/// agent-models-from-agent-evals.md` for the follow-up wiring a `codypendent
/// eval run --suite evals/tasks/regressions --regression` flag would need in
/// `commands.rs` (not owned by this crate module).
pub async fn run_regression_suite(
    paths: &codypendent_protocol::discovery::RuntimePaths,
    cases: &[EvalCase],
    fixture_root: &Path,
) -> anyhow::Result<codypendent_eval::RegressionReport> {
    ensure_daemon(paths).await?;
    let mut suite = codypendent_eval::RegressionSuite::new();
    for case in cases {
        suite.add(case.clone());
    }
    let mut observations = std::collections::BTreeMap::new();
    for case in cases {
        eprintln!("eval: running regression guard {}", case.id);
        let (_scratch, checkout) = checkout_fixture(fixture_root, &case.repository_revision)
            .await
            .with_context(|| {
                format!("preparing the fixture checkout for guard case {}", case.id)
            })?;
        let mut conn = Connection::connect(&paths.socket_path).await?;
        let (mut obs, run_id) = run_case_over_connection(&mut conn, case, &checkout, None).await?;
        inspect_repository(&checkout, run_id, case, &mut obs).await?;
        observations.insert(case.id.clone(), obs);
    }
    let report = suite.evaluate(&observations);
    if report.regressed() {
        eprintln!(
            "eval: regression suite FAILED — {} guard case(s) regressed: {}",
            report.regressed_ids().len(),
            report.regressed_ids().join(", ")
        );
    } else {
        eprintln!(
            "eval: regression suite held — {} guard case(s), none regressed",
            cases.len()
        );
    }
    Ok(report)
}

/// The connected core of one case's headless run: handshake, create a session,
/// attach as `Controller`, start the run, and stream events until it reaches a
/// terminal state — building a [`RunObservation`] from the stream as it goes.
/// Split out (like `commands::run_over_connection`) so a test can drive it
/// against a hand-rolled mock daemon instead of a live one. `routed_model`
/// (when `Some`) is pinned onto the `StartRun` this sends (see [`run_suite`]);
/// `None` sends `StartRun { model: None }`, unchanged.
///
/// Also returns the [`RunId`] the daemon assigned (`None` only if the
/// connection closed before any `RunStarted`/`CommandAccepted` ever named
/// one — an early-failure shape [`inspect_repository`] already handles by
/// falling back to `repository`), so [`run_case`] can locate the run's own
/// isolated worktree (see [`run_worktree_root`]) rather than only ever
/// inspecting `repository` itself.
pub async fn run_case_over_connection(
    conn: &mut Connection,
    case: &EvalCase,
    repository: &Path,
    routed_model: Option<ModelId>,
) -> anyhow::Result<(RunObservation, Option<RunId>)> {
    conn.handshake("codypendent-eval", env!("CARGO_PKG_VERSION"), None)
        .await?;

    let workspace = WorkspaceId::new();
    let create_reply = conn
        .send_command(CommandBody::CreateSession {
            workspace,
            title: format!("eval: {}", case.id),
            repository: Some(repository.to_string_lossy().into_owned()),
            internal: false,
            parent_session_id: None,
            parent_run_id: None,
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
            repository: Some(repository.to_string_lossy().into_owned()),
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
            // `--policy` pins this case's routed model (Phase 7 routing⇄eval
            // composition — see this module's doc); absent `--policy`,
            // `routed_model` is `None` and the daemon resolves/routes as
            // usual, exactly as before.
            model: routed_model,
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
        // Eval runs are headless by definition. Exercise the real approval
        // path, then approve once so cases which legitimately need a gated
        // tool can reach a terminal state instead of waiting forever for a UI
        // that this command never creates. Hard policy denials remain denials
        // and therefore never produce an approval to resolve.
        if let EventBody::ApprovalRequested { approval_id, .. } = &event.body {
            let reply = conn
                .send_command(CommandBody::ResolveApproval {
                    approval_id: *approval_id,
                    decision: ApprovalDecision::Approve,
                    scope: ApprovalScope::Once,
                })
                .await?;
            match reply.payload {
                Payload::CommandAccepted { .. } => {}
                Payload::CommandRejected(error) => anyhow::bail!(
                    "ResolveApproval rejected: {} ({})",
                    error.message,
                    error.code
                ),
                other => anyhow::bail!("unexpected reply to ResolveApproval: {other:?}"),
            }
        }
        let owns_event = matches!(event_run_id(&event.body), Some(rid) if Some(rid) == run_id);
        if owns_event && is_terminal(&event.body) {
            break;
        }
    }
    builder.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok((builder.finish(), run_id))
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
    approved_commands: Vec<String>,
    network_hosts: Vec<String>,
    denied_commands: Vec<String>,
    denied_network_hosts: Vec<String>,
    cost_usd: f64,
    duration_ms: u64,
    /// Whether the run reached `Completed` — see [`RunObservation::run_completed`].
    run_completed: bool,
    /// A proposed action's approval is requested, then resolved, as two
    /// separate events correlated by `approval_id`; a *rejected* proposal never
    /// ran at all, so the action is held here in between and only a
    /// resolved-approve takes it any further.
    pending: HashMap<ApprovalId, ProposedAction>,
    /// The run each approval belongs to. `ToolProposed` is the ONLY event that
    /// links the two — neither `ApprovalRequested` nor `ApprovalResolved`
    /// carries a `run_id` — and it can arrive on either side of the resolution
    /// (the runtime emits it after parking, but the broker emits both approval
    /// events itself when a `Run`-scoped grant auto-approves), so the run is
    /// looked up at completion time rather than assumed known earlier.
    approval_runs: HashMap<ApprovalId, RunId>,
    /// Approved actions still waiting for the `ToolCompleted` that says whether
    /// they actually ran. See [`ObservationBuilder::settle_completion`] for the
    /// correlation and why FIFO-per-run is the right key. Anything left here
    /// when the stream ends is **dropped**: an action whose completion was
    /// never observed has not been shown to have executed.
    awaiting_completion: VecDeque<(ApprovalId, ProposedAction)>,
}

impl ObservationBuilder {
    fn observe(&mut self, body: &EventBody) {
        // Liveness, recorded from whichever terminal signal arrives: a run that
        // failed before the agent acted (no model configured, provider
        // unreachable) satisfies every absence-shaped assertion for free, so
        // the scorer must be able to tell "asserted nothing changed, and
        // nothing did" from "nothing ran". `is_terminal` treats all three
        // dispositions alike because it only decides when to stop reading.
        match body {
            EventBody::RunCompleted {
                disposition: RunDisposition::Completed { .. },
                ..
            }
            | EventBody::RunStateChanged {
                state: RunState::Completed,
                ..
            } => self.run_completed = true,
            _ => {}
        }
        match body {
            // The one event that names both an approval and its run.
            EventBody::ToolProposed {
                run_id,
                approval_id,
                ..
            } => {
                self.approval_runs.insert(*approval_id, *run_id);
            }
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
                        self.awaiting_completion.push_back((*approval_id, action));
                    }
                }
            }
            EventBody::ToolCompleted {
                run_id, outcome, ..
            } => self.settle_completion(*run_id, outcome),
            EventBody::ToolDenied { action, .. } => self.record_denied(action),
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

    /// Record what an approved action makes POSSIBLE — never what it achieved.
    /// A rejected proposal never ran, so it must never reach here at all; an
    /// approved one may still have failed to start, so what lands here is the
    /// deliberately over-broad set the negative assertions read
    /// ([`RunObservation::attempted_commands`], [`RunObservation::network_hosts`]).
    fn record_approved(&mut self, action: &ProposedAction) {
        match action {
            ProposedAction::ExecuteCommand { program, args, .. } => {
                self.approved_commands.push(command_line(program, args));
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

    /// Record an action whose execution a **successful** `ToolCompleted`
    /// confirmed — the only evidence `command-executed` may be scored on.
    fn record_executed(&mut self, action: &ProposedAction) {
        if let ProposedAction::ExecuteCommand { program, args, .. } = action {
            self.executed_commands.push(command_line(program, args));
        }
    }

    /// Match one `ToolCompleted` to the approved action it completes, and
    /// promote that action into the confirmed set only when the tool reported
    /// [`ToolOutcome::Succeeded`].
    ///
    /// **What ties the two together.** An approval and its completion are
    /// separate events and neither names the other, so the key is
    /// `(run_id, order)`: the daemon's agent loop awaits `run_tool` to
    /// completion before dispatching the next call
    /// (`codypendent_runtime::agent::Agent::run_tool`, driven from a sequential
    /// `for` over the step's tool calls), so within one run at most one
    /// approved action is ever outstanding, and every tool call ends in exactly
    /// one `ToolCompleted` for that run. The first still-awaiting action
    /// belonging to this run is therefore this completion's action, even when a
    /// run makes several shell calls. `run_id` comes from `ToolProposed`; an
    /// approval whose `ToolProposed` was never observed matches any run, which
    /// is correct for the one-run-per-session shape `eval run` creates and is
    /// no worse than the FIFO alone.
    ///
    /// A completion with nothing awaiting it is the ordinary case for a tool
    /// that needed no approval at all (`Decision::Allow`), and for the
    /// synthetic failure completions the runtime emits for a policy denial or a
    /// rejected approval — neither of which ever enqueues anything. Nothing to
    /// settle, so nothing is consumed.
    fn settle_completion(&mut self, run_id: RunId, outcome: &ToolOutcome) {
        let approval_runs = &self.approval_runs;
        let Some(index) = self
            .awaiting_completion
            .iter()
            .position(|(approval_id, _)| {
                approval_runs
                    .get(approval_id)
                    .is_none_or(|owner| *owner == run_id)
            })
        else {
            return;
        };
        let Some((_, action)) = self.awaiting_completion.remove(index) else {
            return;
        };
        if matches!(outcome, ToolOutcome::Succeeded) {
            self.record_executed(&action);
        }
    }

    fn record_denied(&mut self, action: &ProposedAction) {
        match action {
            ProposedAction::ExecuteCommand { program, args, .. } => {
                self.denied_commands.push(command_line(program, args));
            }
            ProposedAction::NetworkRequest { destination } => {
                self.denied_network_hosts.push(destination.clone());
            }
            ProposedAction::GitPush { remote, .. } => {
                self.denied_network_hosts.push(remote.clone());
            }
            ProposedAction::GitHubMutation { .. } => {
                self.denied_network_hosts.push("api.github.com".to_string());
            }
            _ => {}
        }
    }

    fn finish(self) -> RunObservation {
        RunObservation {
            approval_requested: self.approval_requested,
            executed_commands: self.executed_commands,
            approved_commands: self.approved_commands,
            denied_commands: self.denied_commands,
            network_hosts: self.network_hosts,
            denied_network_hosts: self.denied_network_hosts,
            cost_usd: self.cost_usd,
            duration_ms: self.duration_ms,
            run_completed: self.run_completed,
            ..Default::default()
        }
    }
}

/// One `ExecuteCommand` as the single line every command assertion's `contains`
/// is matched against: the program, then its arguments, space-separated.
fn command_line(program: &str, args: &[String]) -> String {
    let mut line = program.to_string();
    for arg in args {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

/// Fill in the repository-derived facts an event stream cannot answer:
/// `changed_files` (a working-tree diff against the pinned starting revision,
/// tracked and untracked), and — only when the case actually asserts on them
/// (skipped otherwise to avoid a needless `cargo test`/`git grep`) —
/// `existing_symbols` and `tests_passed`.
///
/// **Inspects the run's own worktree, not blindly `repository`.** A writing
/// run never mutates `repository` itself — see [`run_worktree_root`] for why
/// — so when `run_id` resolves to a worktree that still exists on disk, every
/// fact below is read from THAT directory instead. A case that changed
/// nothing (or whose run never got a `run_id`) still correctly inspects
/// `repository`, because [`run_worktree_root`] returns `None` in exactly
/// that situation (see its own doc for why that fallback is safe, not just
/// convenient).
async fn inspect_repository(
    repository: &Path,
    run_id: Option<RunId>,
    case: &EvalCase,
    obs: &mut RunObservation,
) -> anyhow::Result<()> {
    let root: PathBuf = run_id
        .and_then(|id| run_worktree_root(repository, id))
        .unwrap_or_else(|| repository.to_path_buf());

    obs.changed_files = git_changed_files(&root, &case.repository_revision).await?;
    obs.patch_files_changed = obs.changed_files.len();

    if case
        .expected
        .iter()
        .any(|a| matches!(a, Assertion::SymbolExists { .. }))
    {
        for assertion in &case.expected {
            if let Assertion::SymbolExists { symbol } = assertion {
                if git_grep_has_match(&root, symbol).await? {
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
        obs.tests_passed = Some(run_fixture_tests(&root).await?);
    }

    Ok(())
}

/// Mirrors `codypendent_daemon::worktrees::short_run_id` exactly — 12 lower-hex
/// characters, the tail of the run id's simple (no-hyphen) form. That function
/// is private to the daemon crate and the wire protocol carries no message
/// exposing it, so a client that wants to find its OWN run's isolated
/// worktree (see [`run_worktree_root`]) has no way to ask for the path — it
/// must reconstruct it. Cross-checked against a REAL
/// `codypendent_daemon::worktrees::WorktreeManager::allocate` call by
/// `run_worktree_root_matches_the_daemons_own_layout`, so a future change to
/// either side's formula fails a test here rather than silently going stale.
fn short_run_id(run_id: RunId) -> String {
    let simple = run_id.0.as_simple().to_string();
    simple[simple.len() - 12..].to_string()
}

/// The isolated worktree directory a writing run used, if one still exists on
/// disk — `<repository>/../codypendent-worktrees/<repository-name>/run-<short>`,
/// `codypendent_daemon::worktrees::WorktreeManager`'s own on-disk layout under
/// its default (`--worktree-root` unset) configuration, which every eval run
/// uses (see [`short_run_id`] for why this is reconstructed rather than
/// asked for).
///
/// **Why `repository` itself is the wrong place to look.** STEP 1.8 isolates
/// every writing run onto a dedicated `git worktree` — a sibling directory on
/// its own branch — specifically so a run can never mutate the checkout that
/// spawned it out from under a concurrent reader. `WorktreeManager::release`
/// (called unconditionally once a run ends, success or not) does not merge
/// that branch back into `repository`: it exports the diff as a patch
/// artifact and, whenever the worktree holds unmerged commits or a dirty
/// tree, *retains the directory* rather than deleting it — the "protect
/// unmerged work" exit criterion documented on `WorktreeManager::release`
/// itself. So a case whose run genuinely wrote `src/math.rs` leaves that
/// change sitting in this directory, while `repository`'s own working tree
/// is — correctly, by the runtime's own design — never touched. Diffing
/// `repository` alone (this function's caller's behavior before this fix)
/// therefore reports `file-changed` assertions as failed for every case that
/// actually worked, which is worse than a merely-vacuous pass: it is a
/// false NEGATIVE baked into the harness itself, not the model under test.
///
/// Returns `None` — meaning the caller should inspect `repository` — when
/// nothing is here to find: a read-only run allocates no worktree at all
/// (`WorktreeManager::allocate` is only ever called by the first write
/// attempt), and a run that allocated one but changed nothing has it removed
/// by the same release path (nothing to protect). In both cases `repository`
/// already reflects reality, so falling back to it is not a lesser signal —
/// it is the SAME signal a worktree would have given.
fn run_worktree_root(repository: &Path, run_id: RunId) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(repository).ok()?;
    let parent = canonical.parent()?;
    let repo_name = canonical.file_name()?;
    let candidate = parent
        .join("codypendent-worktrees")
        .join(repo_name)
        .join(format!("run-{}", short_run_id(run_id)));
    candidate.is_dir().then_some(candidate)
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
        // Concurrent fixture checkouts can use the same package name and
        // version. An inherited shared target directory can then execute a
        // different checkout's test binary and turn a known failure into a
        // false pass. Keep compilation artifacts scoped to this checkout.
        .arg("--target-dir")
        .arg(repository.join("target"))
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

/// The `ApprovalResolved` -> `ToolCompleted` correlation: the fix for
/// `command-executed` passing on a command that never succeeded.
#[cfg(test)]
mod execution_correlation_tests {
    use super::*;
    use codypendent_protocol::{Risk, RiskLevel};

    fn shell(program: &str, arg: &str) -> ProposedAction {
        ProposedAction::ExecuteCommand {
            program: program.to_string(),
            args: vec![arg.to_string()],
            environment: Vec::new(),
            cwd: None,
        }
    }

    /// The event sequence a real run emits around one approved shell call, in
    /// the runtime's own order: the broker's `ApprovalRequested`, the agent's
    /// `ToolProposed` (the only event naming both the approval and the run),
    /// the resolution, then the completion — see
    /// `codypendent_runtime::agent::Agent::run_tool`.
    fn approved_call(
        builder: &mut ObservationBuilder,
        run_id: RunId,
        action: ProposedAction,
        outcome: Option<ToolOutcome>,
    ) {
        let approval_id = ApprovalId::new();
        builder.observe(&EventBody::ApprovalRequested {
            approval_id,
            action,
            risk: Risk {
                level: RiskLevel::Medium,
                reasons: Vec::new(),
            },
            pattern: None,
        });
        builder.observe(&EventBody::ToolProposed {
            run_id,
            approval_id,
            action: shell("placeholder", "unused"),
        });
        builder.observe(&EventBody::ApprovalResolved {
            approval_id,
            decision: ApprovalDecision::Approve,
        });
        if let Some(outcome) = outcome {
            builder.observe(&EventBody::ToolCompleted {
                run_id,
                tool: "shell.run".to_string(),
                outcome,
                artifact: None,
            });
        }
    }

    #[test]
    fn a_successful_completion_promotes_the_approved_command() {
        let mut builder = ObservationBuilder::default();
        approved_call(
            &mut builder,
            RunId::new(),
            shell("cat", "src/math.rs"),
            Some(ToolOutcome::Succeeded),
        );
        let obs = builder.finish();
        assert_eq!(obs.executed_commands, vec!["cat src/math.rs".to_string()]);
        assert_eq!(obs.approved_commands, vec!["cat src/math.rs".to_string()]);
    }

    /// THE defect: approval is not execution. A `cat` that exited non-zero must
    /// never satisfy `command-executed`, or the assertion added this round to
    /// stop cases 002/005/007 passing on absence goes right back to passing
    /// without the file ever being read.
    #[test]
    fn a_failed_completion_leaves_the_command_unconfirmed() {
        let mut builder = ObservationBuilder::default();
        approved_call(
            &mut builder,
            RunId::new(),
            shell("cat", "src/math.rs"),
            Some(ToolOutcome::Failed {
                message: "exited 1".to_string(),
            }),
        );
        let obs = builder.finish();
        assert!(
            obs.executed_commands.is_empty(),
            "a non-zero exit is not an execution: {:?}",
            obs.executed_commands
        );
        assert_eq!(
            obs.approved_commands,
            vec!["cat src/math.rs".to_string()],
            "it was still approved, so the negative assertions must still see it"
        );
        assert!(!Assertion::CommandExecuted {
            contains: "src/math.rs".to_string(),
        }
        .check(&obs));
        assert!(!Assertion::CommandNotExecuted {
            contains: "src/math.rs".to_string(),
        }
        .check(&obs));
    }

    /// Fail closed: a completion that never arrived (the run was cancelled
    /// mid-tool, or the connection dropped) leaves the command unconfirmed
    /// rather than assumed successful.
    #[test]
    fn an_approval_with_no_completion_at_all_fails_closed() {
        let mut builder = ObservationBuilder::default();
        approved_call(
            &mut builder,
            RunId::new(),
            shell("cat", "src/math.rs"),
            None,
        );
        let obs = builder.finish();
        assert!(obs.executed_commands.is_empty());
        assert_eq!(obs.approved_commands.len(), 1);
    }

    /// Several shell calls in one run: the per-run FIFO settles each completion
    /// against the call it belongs to, so a failing first call cannot lend its
    /// approval to a succeeding second one (or vice versa).
    #[test]
    fn several_calls_in_one_run_are_settled_in_order() {
        let run_id = RunId::new();
        let mut builder = ObservationBuilder::default();
        approved_call(
            &mut builder,
            run_id,
            shell("cat", "missing.rs"),
            Some(ToolOutcome::Failed {
                message: "no such file".to_string(),
            }),
        );
        approved_call(
            &mut builder,
            run_id,
            shell("cat", "src/math.rs"),
            Some(ToolOutcome::Succeeded),
        );
        let obs = builder.finish();
        assert_eq!(obs.executed_commands, vec!["cat src/math.rs".to_string()]);
        assert_eq!(
            obs.approved_commands,
            vec!["cat missing.rs".to_string(), "cat src/math.rs".to_string()]
        );
    }

    /// A completion belonging to a different run must not settle this run's
    /// outstanding approval — the correlation key is the run, not "the next
    /// completion on the wire".
    #[test]
    fn a_completion_from_another_run_does_not_settle_this_ones_approval() {
        let mine = RunId::new();
        let mut builder = ObservationBuilder::default();
        approved_call(&mut builder, mine, shell("cat", "src/math.rs"), None);
        builder.observe(&EventBody::ToolCompleted {
            run_id: RunId::new(),
            tool: "shell.run".to_string(),
            outcome: ToolOutcome::Succeeded,
            artifact: None,
        });
        assert!(builder.finish().executed_commands.is_empty());
    }

    /// A rejected proposal never reaches either set, and its own synthetic
    /// failure completion consumes nothing.
    #[test]
    fn a_rejected_proposal_is_recorded_nowhere() {
        let run_id = RunId::new();
        let approval_id = ApprovalId::new();
        let mut builder = ObservationBuilder::default();
        builder.observe(&EventBody::ApprovalRequested {
            approval_id,
            action: shell("rm", "-rf"),
            risk: Risk {
                level: RiskLevel::High,
                reasons: Vec::new(),
            },
            pattern: None,
        });
        builder.observe(&EventBody::ToolProposed {
            run_id,
            approval_id,
            action: shell("rm", "-rf"),
        });
        builder.observe(&EventBody::ApprovalResolved {
            approval_id,
            decision: ApprovalDecision::Reject,
        });
        builder.observe(&EventBody::ToolCompleted {
            run_id,
            tool: "shell.run".to_string(),
            outcome: ToolOutcome::Failed {
                message: "approval rejected".to_string(),
            },
            artifact: None,
        });
        let obs = builder.finish();
        assert!(obs.executed_commands.is_empty());
        assert!(obs.approved_commands.is_empty());
        assert!(obs.approval_requested);
    }

    /// A tool the policy engine simply allowed emits `ToolStarted`/
    /// `ToolCompleted` with no approval at all. Its completion must not consume
    /// an unrelated outstanding approval — which sequential tool dispatch
    /// guarantees it cannot, and this pins.
    #[test]
    fn an_unapproved_tools_completion_consumes_nothing() {
        let run_id = RunId::new();
        let mut builder = ObservationBuilder::default();
        builder.observe(&EventBody::ToolCompleted {
            run_id,
            tool: "workspace.read_file".to_string(),
            outcome: ToolOutcome::Succeeded,
            artifact: None,
        });
        approved_call(
            &mut builder,
            run_id,
            shell("cat", "src/math.rs"),
            Some(ToolOutcome::Succeeded),
        );
        assert_eq!(
            builder.finish().executed_commands,
            vec!["cat src/math.rs".to_string()]
        );
    }

    /// The auto-approval ordering: a `Run`-scoped grant makes the broker append
    /// `ApprovalRequested` AND `ApprovalResolved` in one transaction, so
    /// `ToolProposed` — the run link — arrives AFTER the resolution. Looking the
    /// run up at completion time rather than at resolution time is what keeps
    /// that shape working.
    #[test]
    fn an_auto_approved_call_still_correlates_when_tool_proposed_arrives_late() {
        let run_id = RunId::new();
        let approval_id = ApprovalId::new();
        let mut builder = ObservationBuilder::default();
        builder.observe(&EventBody::ApprovalRequested {
            approval_id,
            action: shell("cat", "src/math.rs"),
            risk: Risk {
                level: RiskLevel::Low,
                reasons: Vec::new(),
            },
            pattern: None,
        });
        builder.observe(&EventBody::ApprovalResolved {
            approval_id,
            decision: ApprovalDecision::Approve,
        });
        builder.observe(&EventBody::ToolProposed {
            run_id,
            approval_id,
            action: shell("cat", "src/math.rs"),
        });
        builder.observe(&EventBody::ToolCompleted {
            run_id,
            tool: "shell.run".to_string(),
            outcome: ToolOutcome::Succeeded,
            artifact: None,
        });
        assert_eq!(
            builder.finish().executed_commands,
            vec!["cat src/math.rs".to_string()]
        );
    }

    /// `network_hosts` is deliberately NOT narrowed the way commands were: its
    /// only readers are negative assertions, where an over-broad set is the safe
    /// error. An approved request whose tool then failed must still count as
    /// contact.
    #[test]
    fn an_approved_network_request_counts_as_contact_even_when_the_tool_failed() {
        let mut builder = ObservationBuilder::default();
        approved_call(
            &mut builder,
            RunId::new(),
            ProposedAction::NetworkRequest {
                destination: "evil.example.com".to_string(),
            },
            Some(ToolOutcome::Failed {
                message: "connection refused".to_string(),
            }),
        );
        let obs = builder.finish();
        assert_eq!(obs.network_hosts, vec!["evil.example.com".to_string()]);
        assert!(!Assertion::NoForbiddenNetwork {
            forbidden: vec!["evil.example.com".to_string()],
        }
        .check(&obs));
    }
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
            run_completed: true,
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

    // -- `routed_model_for_case`: the pin `run_suite` sends, one source of --
    // -- truth with what `report_json_with_routing` records --------------

    #[test]
    fn routed_model_for_case_is_none_when_policy_is_absent() {
        // No `--policy` ⇒ `routed: None` ⇒ every case's `StartRun` sends
        // `model: None`, byte-for-byte unchanged (the `eval-smoke` CI path).
        assert_eq!(routed_model_for_case(None, "case-a"), None);
    }

    #[test]
    fn routed_model_for_case_is_none_for_an_unrecognized_case_id() {
        let routed = vec![("case-a".to_string(), ModelId("local-coder".to_string()))];
        assert_eq!(routed_model_for_case(Some(&routed), "case-zzz"), None);
    }

    #[test]
    fn routed_model_for_case_finds_the_matching_case_among_several() {
        let routed = vec![
            ("case-a".to_string(), ModelId("local-coder".to_string())),
            ("case-b".to_string(), ModelId("local-strong".to_string())),
        ];
        assert_eq!(
            routed_model_for_case(Some(&routed), "case-b"),
            Some(ModelId("local-strong".to_string()))
        );
    }

    #[test]
    fn routed_model_for_case_matches_report_json_with_routings_recorded_model() {
        // THE "one source of truth" property (Codex P1 #2 fix): the SAME
        // `routed` pairs feed both the model `run_suite` pins into this
        // case's `StartRun` (via `routed_model_for_case`) and the model
        // `report_json_with_routing` records — so a report's `routed_model`
        // can never name a different model than the one that actually ran.
        let routed = vec![
            ("case-a".to_string(), ModelId("local-coder".to_string())),
            ("case-b".to_string(), ModelId("local-strong".to_string())),
        ];
        let pinned = routed_model_for_case(Some(&routed), "case-a");
        assert_eq!(pinned, Some(ModelId("local-coder".to_string())));

        let case_result = codypendent_eval::CaseResult {
            case_id: "case-a".to_string(),
            assertion_results: vec![],
            within_cost: true,
            within_duration: true,
            run_completed: true,
        };
        let report = SuiteReport::new(vec![case_result]);
        let json = report_json_with_routing(&report, Some(&routed)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let recorded = value["results"][0]["routed_model"]
            .as_str()
            .expect("routed_model is recorded");

        assert_eq!(
            pinned,
            Some(ModelId(recorded.to_string())),
            "the model pinned into this case's StartRun must equal the model \
             recorded in the report"
        );
    }
}

#[cfg(test)]
mod worktree_inspection_tests {
    use super::*;
    use codypendent_daemon::worktrees::WorktreeManager;

    fn git(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args([
                "-c",
                "user.name=eval-worktree-test",
                "-c",
                "user.email=eval-worktree-test@example.com",
            ])
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed: {status}");
    }

    fn init_repo(root: &Path) {
        std::fs::write(root.join("README.md"), "seed\n").unwrap();
        git(root, &["init", "--quiet"]);
        git(root, &["add", "-A"]);
        git(root, &["commit", "--quiet", "-m", "seed"]);
    }

    /// A real cross-check against `codypendent_daemon::worktrees::WorktreeManager`
    /// itself — not just a hand-verified formula — so a future change to the
    /// daemon's own layout (`short_run_id`'s derivation, or the
    /// sibling-directory convention `run_worktree_root` assumes) fails HERE
    /// instead of `codypendent eval run` silently going back to scoring every
    /// writing case's `file-changed`/`tests-pass`/`symbol-exists` assertion
    /// false forever (see `run_worktree_root`'s own doc for the full story:
    /// this is the fix for a false negative baked into the harness itself,
    /// found and verified against a real daemon binary — see the task
    /// report).
    #[tokio::test]
    async fn run_worktree_root_matches_the_daemons_own_layout() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);

        let pool = open_database(&dir.path().join("codypendent.db"))
            .await
            .expect("open a freshly migrated database");
        // `workspace_leases.owner_run_id REFERENCES runs(id)` (migration 0002),
        // and a run references its session — so the lease cannot be allocated
        // for a run id the database has never seen. The daemon always has both
        // rows by the time a writing tool first calls `allocate`.
        let run_id = RunId::new();
        let session_id = codypendent_protocol::SessionId::new();
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query("INSERT INTO sessions (id, title, created_at, updated_at) VALUES (?, ?, ?, ?)")
            .bind(session_id.to_string())
            .bind("eval-worktree-layout")
            .bind(&now)
            .bind(&now)
            .execute(&pool)
            .await
            .expect("seed the owning session");
        sqlx::query(
            "INSERT INTO runs (id, session_id, objective, state, mode, model_policy, budget_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(run_id.to_string())
        .bind(session_id.to_string())
        .bind("eval worktree layout cross-check")
        .bind("Running")
        .bind("Build")
        .bind("hosted-default")
        .bind("{}")
        .execute(&pool)
        .await
        .expect("seed the owning run");

        let lease = WorktreeManager::new()
            .allocate(&pool, &repo, run_id)
            .await
            .expect("allocate a real worktree exactly as the daemon would for a writing run");

        let found = run_worktree_root(&repo, run_id)
            .expect("the freshly allocated worktree exists on disk");
        assert_eq!(
            std::fs::canonicalize(&found).unwrap(),
            std::fs::canonicalize(&lease.worktree_path).unwrap(),
            "run_worktree_root must resolve to the SAME directory WorktreeManager::allocate just created"
        );
    }

    /// `None` — meaning "inspect `repository` itself" — is the correct answer
    /// for a run that never allocated a worktree at all (no writing tool was
    /// ever called; `WorktreeManager::allocate` is only reached by the first
    /// write attempt), not merely "an absent directory".
    #[test]
    fn run_worktree_root_is_none_when_nothing_was_ever_allocated() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        // No `codypendent-worktrees/` sibling exists at all — this is what an
        // eval case whose run touched nothing (or that never got a `run_id`)
        // looks like on disk.
        assert_eq!(run_worktree_root(&repo, RunId::new()), None);
    }

    #[test]
    fn short_run_id_is_the_last_twelve_hex_characters_of_the_simple_form() {
        let run_id = RunId::new();
        let simple = run_id.0.as_simple().to_string();
        assert_eq!(short_run_id(run_id), simple[simple.len() - 12..]);
        assert_eq!(short_run_id(run_id).len(), 12);
    }
}
