//! Councils for the desktop shell.
//!
//! **A council is LOCAL CONFIGURATION, not protocol.** There is no `Council`
//! variant in `CommandBody`; the daemon never hears the word. Definitions live
//! in `<config_dir>/councils.toml` (note: *config* dir, not data dir) and
//! results in `<data_dir>/councils/<name>/*.json|.md`. The TUI reaches them
//! through `codypendent_council::` directly
//! (`crates/cli/src/tui.rs` — `CreateCouncil` at :2721, `DeleteCouncil` at
//! :2782, `RunCouncil` at :2812, `load_council_cards` at :7053), and so does
//! this module. Nothing here re-implements the store, the validation, or the
//! runner: `codypendent-council` is already a dependency of this crate, so the
//! desktop and the TUI cannot disagree about what a council is.
//!
//! Running one is the interesting part. Each member and the chair is an
//! INDEPENDENT daemon session that the desktop's own attached transcript never
//! subscribes to, so `run_with_progress_linked` drives its own connections —
//! exactly the off-thread shape `crates/cli/src/tui.rs:2812` uses — and streams
//! round/member/chair transitions back over a Tauri channel while the command
//! future is still pending.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context};
use codypendent_council::{
    chair_is_member, cost_line, list_definitions, participant_line, persist_definition,
    remove_definition, required_quorum, result_by_id, result_by_name_or_id,
    run_with_progress_linked, CouncilDefinition, CouncilEvent, CouncilMember, CouncilProgress,
    CouncilReport, CouncilReportHandle, CouncilRunFailure, CouncilRunOutcome, StoredCouncilResult,
};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::SessionId;
use serde::{Deserialize, Serialize};

fn paths() -> anyhow::Result<RuntimePaths> {
    RuntimePaths::resolve().context("resolving codypendent runtime paths")
}

/// One member row, as `councils.toml` stores it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CouncilMemberRow {
    pub model: String,
    pub role: String,
}

/// One configured council, projected for the browser.
///
/// `quorum` is the definition's own explicit value and stays `None` when it has
/// none; `requiredQuorum` is what `codypendent_council::required_quorum` will
/// actually enforce. Two fields rather than one because collapsing them would
/// print a number the operator never chose as though they had.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CouncilCard {
    pub name: String,
    pub description: String,
    pub chair: String,
    pub rounds: u8,
    pub evidence: bool,
    pub quorum: Option<usize>,
    pub required_quorum: usize,
    /// The chair also sits as a member — legal, but it then weighs its own
    /// report, which `codypendent_council` warns about at creation time.
    pub chair_is_member: bool,
    pub members: Vec<CouncilMemberRow>,
}

impl From<CouncilDefinition> for CouncilCard {
    fn from(definition: CouncilDefinition) -> Self {
        let required_quorum = required_quorum(&definition);
        let chair_is_member = chair_is_member(&definition);
        Self {
            name: definition.name,
            description: definition.description,
            chair: definition.chair,
            rounds: definition.rounds,
            evidence: definition.evidence,
            quorum: definition.quorum,
            required_quorum,
            chair_is_member,
            members: definition
                .members
                .into_iter()
                .map(|member| CouncilMemberRow {
                    model: member.model,
                    role: member.role,
                })
                .collect(),
        }
    }
}

/// What the builder submits. Deliberately the same fields the TUI's wizard
/// collects and no more: the TUI has NO quorum step and NO evidence step
/// (`crates/cli/src/tui.rs:2735-2743` says so and passes `quorum: None,
/// evidence: false`), so neither is invented here.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CouncilDraft {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub chair: String,
    pub rounds: u8,
    pub members: Vec<CouncilMemberRow>,
}

/// One member's (or the chair's) completed run inside a durable report.
/// `tokens`/`costMicros` are absent when that run measured nothing — never `0`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CouncilMemberOutcomeCard {
    pub model: String,
    pub role: String,
    pub session_id: String,
    pub run_id: String,
    pub response: String,
    pub tokens: Option<u64>,
    pub cost_micros: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CouncilRoundCard {
    pub round: u8,
    pub members: Vec<CouncilMemberOutcomeCard>,
    /// Every failure reason from this round, kept so a partial run is never a
    /// total loss and never looks like a complete one.
    pub failures: Vec<String>,
}

/// A durable council result, as persisted. Mirrors the TUI's
/// `council_stored_summary` (`crates/cli/src/tui.rs:7150`) field for field.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CouncilResultCard {
    pub result_id: String,
    pub council: String,
    /// `completed` | `quorum-failed` | `chair-failed` | `runtime-failed`.
    pub status: String,
    pub objective: String,
    pub started_at: String,
    pub finished_at: String,
    pub repository: String,
    pub origin_session_id: Option<String>,
    pub evidence: bool,
    pub warnings: Vec<String>,
    pub rounds: Vec<CouncilRoundCard>,
    pub failure: Option<String>,
    /// The chair's synthesis. Empty when the chair never ran (a quorum failure);
    /// the `status` and `failure` fields say which, so an empty synthesis is
    /// never mistaken for a chair that answered with nothing.
    pub synthesis: String,
    pub participants: Vec<String>,
    pub cost_line: String,
    pub report_markdown: String,
}

fn result_card(stored: StoredCouncilResult) -> CouncilResultCard {
    let report: CouncilReport = stored.report;
    let handle: CouncilReportHandle = stored.handle;
    let mut participants: Vec<String> = report
        .rounds
        .iter()
        .flat_map(|round| round.members.iter())
        .map(participant_line)
        .collect();
    if let Some(chair) = &report.chair {
        participants.push(participant_line(chair));
    }
    let synthesis = report
        .chair
        .as_ref()
        .map_or_else(String::new, |chair| chair.response.clone());
    let rounds = report
        .rounds
        .into_iter()
        .map(|round| CouncilRoundCard {
            round: round.round,
            members: round
                .members
                .into_iter()
                .map(|member| CouncilMemberOutcomeCard {
                    model: member.model,
                    role: member.role,
                    session_id: member.session_id.to_string(),
                    run_id: member.run_id.to_string(),
                    response: member.response,
                    tokens: member.tokens,
                    cost_micros: member.cost_micros,
                })
                .collect(),
            failures: round.failures,
        })
        .collect();
    CouncilResultCard {
        result_id: handle.result_id.to_string(),
        council: report.council,
        status: report.status,
        objective: report.objective,
        started_at: report.started_at,
        finished_at: report.finished_at,
        repository: report.repository,
        origin_session_id: report.origin_session_id.map(|id| id.to_string()),
        evidence: report.evidence,
        warnings: report.warnings,
        rounds,
        failure: report.failure,
        synthesis,
        participants,
        cost_line: cost_line(&report.costs),
        report_markdown: handle.markdown_path.display().to_string(),
    }
}

/// The fallback projection when a completed run's own report cannot be re-read
/// from disk. Every field is something the run reported; nothing is filled in.
fn run_outcome_card(run: CouncilRunOutcome) -> CouncilResultCard {
    let mut participants: Vec<String> = run.outcome.members.iter().map(participant_line).collect();
    participants.push(participant_line(&run.outcome.chair));
    CouncilResultCard {
        result_id: run.handle.result_id.to_string(),
        council: run.outcome.council,
        status: run.handle.status.clone(),
        objective: run.outcome.objective,
        started_at: run.handle.started_at.clone(),
        finished_at: run.handle.finished_at.clone(),
        repository: run.handle.repository.clone(),
        origin_session_id: run.handle.origin_session_id.map(|id| id.to_string()),
        evidence: false,
        warnings: run.warnings,
        // The durable report holds the per-round detail; this fallback has only
        // the final round's members, so it reports no rounds rather than
        // presenting one round as if it were all of them.
        rounds: Vec::new(),
        failure: None,
        synthesis: run.outcome.chair.response,
        participants,
        cost_line: cost_line(&run.costs),
        report_markdown: run.report_markdown.display().to_string(),
    }
}

/// The council browser's projection: every configured definition.
pub fn list_councils() -> anyhow::Result<Vec<CouncilCard>> {
    Ok(list_definitions(&paths()?)?
        .into_iter()
        .map(CouncilCard::from)
        .collect())
}

/// Validate and persist a new definition.
///
/// Validation is `codypendent_council::persist_definition`'s own — name charset
/// and path safety, 2..=MAX members, unique member models, and every member
/// model plus the chair having to already exist in `models.toml`. None of it is
/// duplicated here, so a refusal the TUI gives is the refusal the desktop gives.
pub fn create_council(draft: CouncilDraft) -> anyhow::Result<CouncilCard> {
    let definition = CouncilDefinition {
        name: draft.name.trim().to_owned(),
        description: draft.description.trim().to_owned(),
        chair: draft.chair.trim().to_owned(),
        rounds: draft.rounds,
        // No quorum step in the wizard, so take the default majority rule
        // rather than pinning a number the operator was never shown.
        quorum: None,
        // No evidence step either; a council created here keeps the default and
        // can be flipped on by editing councils.toml or via the CLI's
        // `--evidence` flag, exactly as with the TUI wizard.
        evidence: false,
        // On by default for every council however it was created; the wizard
        // has no step to disable it because a check you must remember to turn
        // on is off exactly when it would have mattered.
        review: true,
        // No explicit reviewer: the run picks a member that is not the chair.
        reviewer: None,
        members: draft
            .members
            .into_iter()
            .map(|member| CouncilMember {
                model: member.model.trim().to_owned(),
                role: member.role.trim().to_owned(),
            })
            .collect(),
    };
    Ok(CouncilCard::from(persist_definition(
        &paths()?,
        definition,
    )?))
}

/// Remove a definition. Saved run reports are deliberately left on disk — the
/// TUI's `DeleteCouncil` does the same, because a deliberation that happened
/// still happened.
pub fn delete_council(name: &str) -> anyhow::Result<()> {
    remove_definition(&paths()?, name)
}

/// The results browser. `warnings` carries per-council read failures so one
/// unreadable report degrades that row rather than emptying the page.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CouncilResultsPage {
    pub results: Vec<CouncilResultCard>,
    pub warnings: Vec<String>,
}

/// Every council's newest durable result, newest first.
///
/// Ported from `crates/cli/src/tui.rs:7208 load_council_result_cards`: a
/// missing `<data_dir>/councils` directory is an empty page (no council has
/// ever run), which is NOT the same as a failure — a failure returns `Err` and
/// the view says unavailable.
pub fn list_council_results() -> anyhow::Result<CouncilResultsPage> {
    let paths = paths()?;
    let root = paths.data_dir.join("councils");
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CouncilResultsPage {
                results: Vec::new(),
                warnings: Vec::new(),
            })
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", root.display()));
        }
    };
    let mut results = Vec::new();
    let mut warnings = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        match codypendent_council::latest_result(&paths, &name) {
            Ok(Some(stored)) => results.push(result_card(stored)),
            Ok(None) => {}
            Err(error) => {
                warnings.push(format!("could not load council result `{name}`: {error:#}"));
            }
        }
    }
    results.sort_by(|left, right| right.finished_at.cmp(&left.finished_at));
    Ok(CouncilResultsPage { results, warnings })
}

/// One durable result by selector: a UUID is a result id, anything else is a
/// council name resolving to that council's newest result. `Ok(None)` means
/// "looked, nothing there" and is distinct from `Err`.
pub fn council_result(selector: &str) -> anyhow::Result<Option<CouncilResultCard>> {
    Ok(result_by_name_or_id(&paths()?, selector)?.map(result_card))
}

/// One streamed progress line from a running council.
///
/// `phase` is `CouncilEvent::phase()` — the crate's own vocabulary, not a
/// desktop invention. `activeSubagents` mirrors the TUI's own tally
/// (`crates/cli/src/tui.rs:2820`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CouncilProgressFrame {
    pub council: String,
    pub result_id: String,
    pub phase: String,
    pub occurred_at: String,
    pub message: String,
    pub active_subagents: usize,
}

/// The same single-line wording the CLI's `council run` prints and the TUI's
/// transcript shows (`crates/cli/src/tui.rs:7082 council_progress_message`), so
/// the three surfaces read identically.
fn progress_message(event: &CouncilEvent) -> String {
    match event {
        CouncilEvent::RoundStarted {
            round,
            rounds,
            members,
        } => format!("round {round}/{rounds} — launching {members} member(s)"),
        CouncilEvent::MemberCompleted { round, role, model } => {
            format!("round {round} — {role} ({model}) completed")
        }
        CouncilEvent::MemberFailed { round, error } => {
            format!("round {round} — member failed: {error}")
        }
        CouncilEvent::ChairRuled { round } => format!("chair ruled on round {round}"),
        CouncilEvent::ReviewStarted { reviewer } => {
            format!("independent reviewer `{reviewer}` reading the synthesis")
        }
        CouncilEvent::ChairStarted { chair } => format!("asking chair `{chair}` to synthesize"),
        CouncilEvent::Warning { message } => format!("warning: {message}"),
    }
}

/// Where a running council's progress lines go. A Tauri channel in the bridge;
/// a plain collector in tests.
pub trait ProgressSink: Send + Sync + 'static {
    fn emit(&self, frame: CouncilProgressFrame);
}

/// What a finished council run hands back.
///
/// EVERY persisted outcome comes back as a `result`, including a quorum or
/// chair failure — `codypendent_council` writes a report for those too, and
/// discarding it would throw away the members' completed work. `failure` is
/// `Some` exactly when the run did not complete, so a partial report can never
/// be read as a successful one.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CouncilRunReply {
    pub result: Option<CouncilResultCard>,
    pub failure: Option<String>,
}

/// Run a persisted council against `objective`.
///
/// `repository` must already be a validated checkout root
/// (`crate::repository::validate_repository`); `codypendent_council` canonicalizes
/// it and refuses a non-directory, but the desktop never asks it to guess.
///
/// `evidence: false` is passed always: `run_with_progress_linked` ORs it with
/// the council's OWN stored flag, so this defers to the definition and adds no
/// per-run override the operator was not offered.
pub async fn run_council<S: ProgressSink>(
    name: String,
    objective: String,
    repository: PathBuf,
    origin_session_id: Option<SessionId>,
    sink: Arc<S>,
) -> anyhow::Result<CouncilRunReply> {
    let paths = paths()?;

    // `codypendent_council::run_with_progress_linked` calls `ensure_daemon`,
    // which SPAWNS `std::env::current_exe() __daemon` when no daemon answers.
    // In this shell `current_exe()` is the desktop binary, so that branch would
    // launch a second window instead of a daemon. Refuse first, explicitly,
    // rather than letting it be reached.
    if tokio::net::UnixStream::connect(&paths.socket_path)
        .await
        .is_err()
    {
        bail!(
            "no daemon is listening on {}. A council convenes real member and chair runs, so \
             it needs a running codypendentd — connect the desktop client first.",
            paths.socket_path.display()
        );
    }

    let council = name.clone();
    let active = Arc::new(AtomicUsize::new(0));
    let progress = move |progress: CouncilProgress| {
        let active_subagents = match &progress.event {
            CouncilEvent::RoundStarted { members, .. } => {
                active.store(*members, Ordering::Relaxed);
                *members
            }
            CouncilEvent::MemberCompleted { .. } | CouncilEvent::MemberFailed { .. } => active
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    Some(value.saturating_sub(1))
                })
                .unwrap_or(0)
                .saturating_sub(1),
            // The chair working alone, whether ruling between rounds or
            // synthesizing at the end.
            CouncilEvent::ChairRuled { .. }
            | CouncilEvent::ReviewStarted { .. }
            | CouncilEvent::ChairStarted { .. } => {
                active.store(1, Ordering::Relaxed);
                1
            }
            CouncilEvent::Warning { .. } => active.load(Ordering::Relaxed),
        };
        sink.emit(CouncilProgressFrame {
            council: council.clone(),
            result_id: progress.result_id.to_string(),
            phase: progress.event.phase().to_owned(),
            occurred_at: progress.occurred_at,
            message: progress_message(&progress.event),
            active_subagents,
        });
    };

    match run_with_progress_linked(
        &paths,
        &name,
        objective,
        repository,
        origin_session_id,
        false,
        progress,
    )
    .await
    {
        Ok(run) => {
            // Prefer the durable report: it carries every round, every failure
            // reason and the measured costs. The in-memory outcome is the
            // fallback for the case where the report cannot be re-read.
            let result = match result_by_id(&paths, run.handle.result_id) {
                Ok(Some(stored)) => result_card(stored),
                _ => run_outcome_card(run),
            };
            Ok(CouncilRunReply {
                result: Some(result),
                failure: None,
            })
        }
        Err(error) => {
            let message = format!("{error:#}");
            // A quorum or chair failure still persisted a report naming every
            // member that DID complete. Hand it back alongside the failure.
            let handle = error
                .downcast_ref::<CouncilRunFailure>()
                .map(|failure| failure.handle.clone());
            let result = handle
                .and_then(|handle| result_by_id(&paths, handle.result_id).ok().flatten())
                .map(result_card);
            match result {
                Some(result) => Ok(CouncilRunReply {
                    result: Some(result),
                    failure: Some(message),
                }),
                // Nothing was persisted (a validation refusal, a missing
                // council). There is no partial answer to show.
                None => Err(error),
            }
        }
    }
}

/// Resolve a repository string into the path a council run is anchored to.
/// A council run indexes and reads a real checkout, so an unselected or invalid
/// repository is refused here rather than defaulting to the process directory.
pub fn council_repository(repository: Option<&str>) -> anyhow::Result<PathBuf> {
    match repository {
        Some(path) if !path.trim().is_empty() => Ok(PathBuf::from(
            crate::repository::validate_repository(Path::new(path))?.path,
        )),
        _ => match crate::repository::selected_repository()? {
            Some(selection) => Ok(PathBuf::from(selection.path)),
            None => bail!(
                "no repository is selected. A council's members run against a checkout, so \
                 choose one before convening."
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::str::FromStr as _;

    struct Collector(std::sync::Mutex<Vec<CouncilProgressFrame>>);

    impl ProgressSink for Collector {
        fn emit(&self, frame: CouncilProgressFrame) {
            self.0.lock().expect("collector mutex").push(frame);
        }
    }

    #[test]
    fn progress_wording_matches_the_cli_and_tui() {
        assert_eq!(
            progress_message(&CouncilEvent::RoundStarted {
                round: 1,
                rounds: 2,
                members: 3
            }),
            "round 1/2 — launching 3 member(s)"
        );
        assert_eq!(
            progress_message(&CouncilEvent::MemberCompleted {
                round: 2,
                role: "critic".to_owned(),
                model: "gpt".to_owned()
            }),
            "round 2 — critic (gpt) completed"
        );
        assert_eq!(
            progress_message(&CouncilEvent::ChairStarted {
                chair: "opus".to_owned()
            }),
            "asking chair `opus` to synthesize"
        );
    }

    /// A sink is only a sink — it does not decide anything.
    #[test]
    fn a_collector_records_what_it_is_given() {
        let collector = Collector(std::sync::Mutex::new(Vec::new()));
        collector.emit(CouncilProgressFrame {
            council: "review".to_owned(),
            result_id: "id".to_owned(),
            phase: "warning".to_owned(),
            occurred_at: "now".to_owned(),
            message: "warning: chair is also a member".to_owned(),
            active_subagents: 0,
        });
        assert_eq!(collector.0.lock().expect("mutex").len(), 1);
    }

    /// Refusing an unselected repository is the whole point: a council must
    /// never fall back to the process working directory.
    #[test]
    fn a_blank_repository_is_refused_when_none_is_selected() {
        // Only assert the refusal shape when nothing is selected; a developer
        // machine with a real selection legitimately resolves it.
        if crate::repository::selected_repository()
            .ok()
            .flatten()
            .is_none()
        {
            let error = council_repository(Some("   ")).expect_err("must refuse");
            assert!(format!("{error:#}").contains("no repository is selected"));
        }
    }

    #[test]
    fn a_draft_keeps_the_wizards_defaults() {
        let draft = CouncilDraft {
            name: " review ".to_owned(),
            description: String::new(),
            chair: "chair-model".to_owned(),
            rounds: 1,
            members: vec![CouncilMemberRow {
                model: "a".to_owned(),
                role: "member".to_owned(),
            }],
        };
        // Not persisted (that needs configured models); this only pins that the
        // draft carries no quorum/evidence knobs the wizard never showed.
        assert_eq!(draft.name.trim(), "review");
        assert_eq!(draft.rounds, 1);
    }

    #[test]
    fn a_session_id_selector_parses_or_is_absent() {
        assert!(SessionId::from_str("not-a-uuid").is_err());
    }
}
