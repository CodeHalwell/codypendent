//! Headless session rendering: readable answers for `codypendent run`, and
//! one self-describing JSON `Envelope` per stdout line for `run --jsonl` and
//! `attach --events jsonl`. Both projections consume the same daemon events
//! and share run ownership, terminal disposition, and exit-code handling.

use std::io::Write;

use anyhow::{anyhow, Context};
use codypendent_protocol::{
    Catchup, ClientId, Envelope, EventBody, MessageId, Payload, RunDisposition, RunId, RunState,
    SessionEvent, SessionId, PROTOCOL_V1,
};

use crate::connection::Connection;

/// Human-readable answers by default; lossless envelopes for automation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Jsonl,
}

#[derive(Default)]
struct TextOutput {
    wrote_answer: bool,
    line_open: bool,
}

impl TextOutput {
    fn write<W: Write>(&mut self, out: &mut W, text: &str) -> anyhow::Result<()> {
        let safe = codypendent_tui::sanitize_accessible_text(text);
        if !safe.is_empty() {
            write!(out, "{safe}").context("writing the answer")?;
            out.flush().context("flushing the answer")?;
            self.wrote_answer = true;
            self.line_open = !safe.ends_with('\n');
        }
        Ok(())
    }

    fn finish_line<W: Write>(&mut self, out: &mut W) -> anyhow::Result<()> {
        if self.line_open {
            writeln!(out).context("finishing the answer")?;
            out.flush()?;
            self.line_open = false;
        }
        Ok(())
    }

    fn event<W: Write, E: Write>(
        &mut self,
        out: &mut W,
        diagnostics: &mut E,
        body: &EventBody,
    ) -> anyhow::Result<()> {
        match body {
            EventBody::ModelStreamDelta {
                text,
                thought: false,
                ..
            } => self.write(out, text)?,
            EventBody::ToolStarted { tool, label, .. } => {
                self.finish_line(out)?;
                writeln!(
                    diagnostics,
                    "codypendent: running tool {}",
                    codypendent_tui::sanitize_accessible_text(if tool.starts_with("acp:") {
                        label.as_deref().unwrap_or("Tool")
                    } else {
                        tool
                    })
                )?;
            }
            EventBody::ToolUpdated {
                label: Some(label), ..
            } => {
                writeln!(
                    diagnostics,
                    "codypendent: tool action {}",
                    codypendent_tui::sanitize_accessible_text(label)
                )?;
            }
            EventBody::ModelRetrying {
                attempt,
                max_attempts,
                message,
                delay_ms,
                ..
            } => {
                writeln!(
                    diagnostics,
                    "codypendent: retry {attempt}/{max_attempts} in {}s — {}",
                    delay_ms.div_ceil(1000),
                    codypendent_tui::state::sanitize_failure_text(message)
                )?;
            }
            EventBody::RunStateChanged {
                state: RunState::WaitingForApproval,
                ..
            } => {
                self.finish_line(out)?;
                writeln!(diagnostics, "codypendent: waiting for approval; open this session in the TUI or desktop to review it")?;
            }
            EventBody::RunStateChanged {
                state: RunState::WaitingForUserInput,
                ..
            } => {
                self.finish_line(out)?;
                writeln!(diagnostics, "codypendent: waiting for your answer; open this session in the TUI or desktop to respond")?;
            }
            EventBody::RunUsage {
                prompt_tokens,
                completion_tokens,
                cost_micros,
                coverage,
                ..
            } => {
                if let Some(label) = codypendent_tui::usage::format_usage(
                    *prompt_tokens,
                    *completion_tokens,
                    *cost_micros,
                    *coverage,
                ) {
                    self.finish_line(out)?;
                    writeln!(diagnostics, "codypendent: measured usage: {label}")?;
                    diagnostics.flush()?;
                }
            }
            EventBody::RunCompleted {
                disposition:
                    RunDisposition::Completed {
                        summary: Some(summary),
                    },
                ..
            } if !self.wrote_answer => self.write(out, summary)?,
            _ => {}
        }
        Ok(())
    }
}

/// The terminal disposition of a headless run — the STEP 1.13 exit-code
/// contract for `codypendent run --jsonl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunExit {
    Completed,
    Failed,
    Cancelled,
}

impl RunExit {
    /// `0` on `Completed`, `2` on `Failed`, `130` on `Cancelled` — exactly
    /// STEP 1.13's contract. `main` is the only place that calls
    /// `std::process::exit`; every other layer just returns this value.
    pub fn exit_code(self) -> i32 {
        match self {
            RunExit::Completed => 0,
            RunExit::Failed => 2,
            RunExit::Cancelled => 130,
        }
    }

    fn from_state(state: RunState) -> Option<Self> {
        match state {
            RunState::Completed => Some(RunExit::Completed),
            RunState::Failed => Some(RunExit::Failed),
            RunState::Cancelled => Some(RunExit::Cancelled),
            _ => None,
        }
    }

    fn from_disposition(disposition: &RunDisposition) -> Option<Self> {
        match disposition {
            RunDisposition::Completed { .. } => Some(RunExit::Completed),
            RunDisposition::Failed { .. } => Some(RunExit::Failed),
            RunDisposition::Cancelled { .. } => Some(RunExit::Cancelled),
            // `Unknown`, and any future non_exhaustive variant (RULE 1).
            _ => None,
        }
    }
}

/// The human-readable text worth echoing to stderr for a terminal
/// disposition — `Failed`'s `reason` always, `Cancelled`'s `reason` when the
/// canceller gave one, and nothing for a plain `Completed` (a summary there
/// is routine output, not a diagnostic; the JSONL line already carries it for
/// a caller that wants it).
fn disposition_reason(disposition: &RunDisposition) -> Option<&str> {
    match disposition {
        RunDisposition::Failed { reason, .. } => Some(reason.as_str()),
        RunDisposition::Cancelled {
            reason: Some(reason),
        } => Some(reason.as_str()),
        _ => None,
    }
}

/// Past-tense verb for the stderr summary line.
fn exit_verb(exit: RunExit) -> &'static str {
    match exit {
        RunExit::Completed => "completed",
        RunExit::Failed => "failed",
        RunExit::Cancelled => "cancelled",
    }
}

/// Write one JSONL line: `serde_json::to_string(&envelope)` + `\n`, flushed
/// immediately so a consuming pipe observes each event as it arrives rather
/// than waiting for a buffer to fill.
fn write_line<W: Write>(out: &mut W, envelope: &Envelope) -> anyhow::Result<()> {
    let line = serde_json::to_string(envelope).context("serializing an event envelope")?;
    writeln!(out, "{line}").context("writing a JSONL line")?;
    out.flush().context("flushing the JSONL stream")?;
    Ok(())
}

/// Wrap a bare `SessionEvent` from a `Catchup::Events` reply in the same
/// `Envelope` shape a live-forwarded event arrives in (mirrors
/// `crates/daemon/src/server.rs`'s `forward_events`, which stamps
/// `session_id` on the envelope it forwards), so every JSONL line — catch-up
/// or live — is an independently parseable `Envelope`, never a bare
/// `SessionEvent`.
fn envelope_for(client_id: ClientId, session_id: SessionId, event: SessionEvent) -> Envelope {
    Envelope {
        protocol_version: PROTOCOL_V1,
        message_id: MessageId::new(),
        correlation_id: None,
        client_id,
        workspace_id: None,
        session_id: Some(session_id),
        sequence: Some(event.sequence),
        payload: Payload::Event(event),
    }
}

/// Replay an attach-time `Catchup` as JSONL lines. `Catchup::Events` replays
/// each missed `SessionEvent` in order; `Catchup::Snapshot` (the client was
/// too far behind — Chapter 03's >500-events rule) carries a projection, not
/// individual events, so it emits the snapshot envelope itself as one JSONL
/// line. This preserves actionable projection state such as pending approvals
/// for headless consumers. A future `Catchup::Unknown` variant is skipped.
pub fn replay_catchup<W: Write>(
    out: &mut W,
    client_id: ClientId,
    session_id: SessionId,
    catchup: Catchup,
) -> anyhow::Result<()> {
    match catchup {
        Catchup::Events { events, .. } => {
            for event in events {
                write_line(out, &envelope_for(client_id, session_id, event))?;
            }
        }
        snapshot @ Catchup::Snapshot { through, .. } => {
            let mut envelope = Envelope::request(client_id, Payload::Catchup { catchup: snapshot });
            envelope.session_id = Some(session_id);
            envelope.sequence = Some(through);
            write_line(out, &envelope)?;
        }
        _ => {}
    }
    Ok(())
}

/// Stream live events to `out` as JSONL until a terminal run event arrives,
/// returning the mapped [`RunExit`]. Used by `codypendent run --jsonl`, which
/// attaches to a session it just started exactly one run in.
///
/// Once the first `RunStarted` is observed, its `run_id` is remembered; every
/// event is still forwarded to `out` (a client sees everything it is
/// subscribed to — Chapter 03), but only an event belonging to *that* run can
/// end the stream. This matters if a second client concurrently starts a
/// second run in the same session: STEP 1.13 defines `run`'s exit code for
/// the run it itself started, not for whichever run happens to finish first.
///
/// `RunCompleted { disposition, .. }`, not `RunStateChanged { state, .. }`, is
/// what ends the stream. Every terminal path in the daemon (the plain agent
/// loop's own finish, `crates/runtime/src/agent.rs`; the ACP bridge's
/// `finish_acp_run`; and `crates/daemon/src/recovery.rs::fail_run`, reached
/// when a run cannot even start — no model configured, every candidate
/// unreachable, ...) persists+publishes a `RunStateChanged` carrying only the
/// bare terminal `state`, immediately followed by a `RunCompleted` carrying
/// the full [`RunDisposition`] — the human-readable `reason` (`Failed`) or
/// `summary` (`Completed`). Both are forwarded to `out` either way, so a
/// client reading the raw stream still sees the bare state change; what
/// changed is which one this function treats as authoritative for the
/// returned [`RunExit`] and, before this fix, for ending the stream at all —
/// returning on the first (`RunStateChanged`) closed the connection before
/// the richer `RunCompleted` ever arrived, so a scripted `--jsonl` caller saw
/// a bare `Failed` with no reason anywhere in its own output (the reason
/// existed only in `daemon.log`). If the connection ends before a matching
/// `RunCompleted` arrives — a terminal `RunStateChanged` was seen but nothing
/// followed it (an older daemon, or a crash between the two publishes) — that
/// remembered state is the fallback, so this still cannot regress into a hang
/// where it previously exited.
pub async fn stream_until_terminal<W: Write>(
    conn: &mut Connection,
    out: &mut W,
    expected_run: Option<RunId>,
) -> anyhow::Result<RunExit> {
    stream_until_terminal_with_format(conn, out, expected_run, OutputFormat::Jsonl).await
}

pub async fn stream_until_terminal_with_format<W: Write>(
    conn: &mut Connection,
    out: &mut W,
    expected_run: Option<RunId>,
    format: OutputFormat,
) -> anyhow::Result<RunExit> {
    stream_until_terminal_with_writers(conn, out, &mut std::io::stderr(), expected_run, format)
        .await
}

/// The same stream with an explicit diagnostic sink, keeping answers/JSONL on
/// stdout and progress/usage on stderr. Also permits end-to-end capture tests.
pub async fn stream_until_terminal_with_writers<W: Write, E: Write>(
    conn: &mut Connection,
    out: &mut W,
    diagnostics: &mut E,
    expected_run: Option<RunId>,
    format: OutputFormat,
) -> anyhow::Result<RunExit> {
    let mut text_output = TextOutput::default();
    // The authoritative binding is the run id the daemon reported for OUR
    // StartRun; first-observed `RunStarted` is only the older-daemon fallback.
    let mut run_id: Option<RunId> = expected_run;
    // Set once a terminal `RunStateChanged` for `run_id` is observed; used
    // only if the connection ends before `RunCompleted` follows it.
    let mut pending_exit: Option<RunExit> = None;
    // Current daemons persist measured usage before the terminal barrier. Keep
    // the bounded trailing drain only for older daemons that emitted it after.
    let mut saw_usage = false;
    loop {
        let Some(envelope) = conn.next_envelope().await? else {
            text_output.finish_line(out)?;
            return pending_exit.ok_or_else(|| {
                anyhow!("daemon closed the connection before the run reached a terminal state")
            });
        };
        let Payload::Event(event) = &envelope.payload else {
            continue; // not an Event payload (e.g. a stray reply); ignore
        };
        if let EventBody::RunStarted { run_id: rid, .. } = &event.body {
            run_id.get_or_insert(*rid);
        }

        let owns_event = matches!(event_run_id(&event.body), Some(rid) if Some(rid) == run_id);
        match format {
            OutputFormat::Jsonl => write_line(out, &envelope)?,
            OutputFormat::Text if owns_event => text_output.event(out, diagnostics, &event.body)?,
            OutputFormat::Text => {}
        }
        if !owns_event {
            continue;
        }
        match &event.body {
            EventBody::RunCompleted { disposition, .. } => {
                if let Some(exit) = RunExit::from_disposition(disposition) {
                    // The JSONL line already written above carries the full
                    // disposition (a scripted consumer can read `reason` off
                    // it directly), but a non-Failed exit code alone tells a
                    // human running this interactively nothing about why —
                    // and stdout's "nothing but JSONL" contract means it
                    // cannot go there, so stderr is the only place left for
                    // it. Mirrors the daemon-mismatch warning's stderr-only
                    // convention just above this function's caller.
                    if let Some(reason) = disposition_reason(disposition) {
                        writeln!(
                            diagnostics,
                            "codypendent: run {} — {}",
                            exit_verb(exit),
                            codypendent_tui::state::sanitize_failure_text(reason)
                        )?;
                    }
                    text_output.finish_line(out)?;
                    if !saw_usage {
                        drain_trailing_usage(
                            conn,
                            out,
                            diagnostics,
                            run_id,
                            format,
                            &mut text_output,
                        )
                        .await?;
                    }
                    return Ok(exit);
                }
            }
            EventBody::RunUsage { .. } => saw_usage = true,
            EventBody::RunStateChanged { state, .. } => {
                pending_exit = RunExit::from_state(*state).or(pending_exit);
            }
            _ => {}
        }
    }
}

/// How long [`drain_trailing_usage`] waits for the run's `RunUsage` after its
/// terminal event. Generous relative to the gap it covers — the daemon emits
/// both from the same `finish_run` continuation, microseconds apart — and short
/// enough that a daemon which never emits one costs a scripted caller a barely
/// perceptible pause rather than a hang.
const TRAILING_USAGE_GRACE: std::time::Duration = std::time::Duration::from_millis(1500);

/// After the run's terminal event, keep reading just long enough to forward its
/// `RunUsage`.
///
/// Current daemons publish measured usage before `RunCompleted`, but older
/// compatible daemons published it one sequence afterward. Returning
/// immediately on completion dropped that final JSONL line, so this bounded
/// compatibility drain remains for streams where no matching usage was seen.
///
/// Bounded and best-effort in three ways, because a usage event is not
/// guaranteed: the daemon emits none when the provider measured nothing (an
/// all-`None` event would be indistinguishable from a genuinely free run), an
/// older daemon emits none at all, and the connection may simply end. None of
/// those may change the run's exit code, so every exit here is `Ok(())`.
async fn drain_trailing_usage<W: Write, E: Write>(
    conn: &mut Connection,
    out: &mut W,
    diagnostics: &mut E,
    run_id: Option<RunId>,
    format: OutputFormat,
    text_output: &mut TextOutput,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + TRAILING_USAGE_GRACE;
    loop {
        let envelope = match tokio::time::timeout_at(deadline, conn.next_envelope()).await {
            // Elapsed, or the connection ended: nothing more is coming.
            Err(_elapsed) => return Ok(()),
            Ok(Ok(Some(envelope))) => envelope,
            Ok(Ok(None)) => return Ok(()),
            // A read error after a completed run must not turn a successful run
            // into a failed one — the disposition is already decided.
            Ok(Err(_error)) => return Ok(()),
        };
        let Payload::Event(event) = &envelope.payload else {
            continue;
        };
        // Forward whatever arrives in the window, exactly as the main loop
        // does: the JSONL contract is "every event the daemon sent us", not
        // "the events this function happens to care about".
        if format == OutputFormat::Jsonl {
            write_line(out, &envelope)?;
        }
        if matches!(event.body, EventBody::RunUsage { .. })
            && matches!(event_run_id(&event.body), Some(rid) if Some(rid) == run_id)
        {
            if format == OutputFormat::Text {
                text_output.event(out, diagnostics, &event.body)?;
            }
            return Ok(());
        }
    }
}

/// Stream live events to `out` as JSONL forever, returning only when the
/// connection ends (the session closed, or the daemon dropped the client).
/// Used by `codypendent attach --events jsonl`, which the caller races against
/// Ctrl-C (`tokio::select!` in `crate::commands::attach`).
pub async fn stream_forever<W: Write>(conn: &mut Connection, out: &mut W) -> anyhow::Result<()> {
    loop {
        let Some(envelope) = conn.next_envelope().await? else {
            return Ok(());
        };
        if matches!(envelope.payload, Payload::Event(_)) {
            write_line(out, &envelope)?;
        }
    }
}

/// The run a run-scoped event belongs to, if any. Mirrors
/// `crates/daemon/src/server.rs`'s private `event_run_id` (duplicated rather
/// than shared: the CLI must not depend on `codypendent-daemon`). `pub(crate)`
/// (rather than private) so `crate::eval`'s suite runner can reuse the exact
/// same run-ownership rule `stream_until_terminal` uses, instead of a second
/// copy drifting from this one within the same crate.
///
/// Outcome 20 (F-20-3): `ToolDenied` is run-scoped exactly like
/// `ToolProposed`/`ToolStarted` — a policy denial that is missing here is a
/// policy denial `stream_until_terminal`'s caller and `crate::eval`'s suite
/// runner can never see, even though the daemon journaled it under this run's
/// id. Keep this list in sync with the server's copy; both must omit exactly
/// the session-scoped (non-run) event bodies, never a run-scoped one.
///
/// `RunUsage` and `LearningsCaptured` are here for that same reason: the
/// server's copy resolves both to a run, and a copy that disagrees makes the
/// CLI's notion of "this run's events" narrower than the daemon's — the drift
/// this doc comment exists to forbid. The unit test below pins the rule so a
/// new run-scoped variant cannot land in one copy only.
pub(crate) fn event_run_id(body: &EventBody) -> Option<RunId> {
    match body {
        EventBody::RunStarted { run_id, .. }
        | EventBody::RunStateChanged { run_id, .. }
        | EventBody::ModelStreamDelta { run_id, .. }
        | EventBody::ModelRetrying { run_id, .. }
        | EventBody::ToolProposed { run_id, .. }
        | EventBody::ToolDenied { run_id, .. }
        | EventBody::ToolStarted { run_id, .. }
        | EventBody::ToolUpdated { run_id, .. }
        | EventBody::ToolCompleted { run_id, .. }
        | EventBody::PatchProposed { run_id, .. }
        | EventBody::SteeringQueued { run_id }
        | EventBody::SteeringApplied { run_id }
        | EventBody::BudgetWarning { run_id, .. }
        | EventBody::RunCompleted { run_id, .. }
        | EventBody::RunUsage { run_id, .. }
        | EventBody::ContextUsage { run_id, .. }
        | EventBody::LearningsCaptured { run_id, .. } => Some(*run_id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use codypendent_protocol::{
        ArtifactId, ArtifactRef, BudgetDimension, DataClassification, RunDisposition, RunState,
    };

    /// Every run-scoped [`EventBody`] this build knows, each carrying `run`.
    ///
    /// The guard below is a list rather than a derived enumeration because Rust
    /// cannot iterate a non-`unit` enum's variants at runtime; adding a
    /// run-scoped variant means adding it here, which is exactly the moment to
    /// notice the two `event_run_id` copies must move together.
    fn run_scoped_bodies(run: RunId) -> Vec<EventBody> {
        let artifact = ArtifactRef {
            id: ArtifactId::new(),
            media_type: "text/markdown".to_owned(),
            byte_length: 12,
            sha256: "a".repeat(64),
            sensitivity: DataClassification::Internal,
        };
        vec![
            EventBody::RunStateChanged {
                run_id: run,
                state: RunState::Running,
            },
            EventBody::ModelStreamDelta {
                run_id: run,
                text: String::new(),
                thought: false,
            },
            EventBody::SteeringQueued { run_id: run },
            EventBody::SteeringApplied { run_id: run },
            EventBody::BudgetWarning {
                run_id: run,
                dimension: BudgetDimension::WallClock,
                used: 1,
                limit: 2,
            },
            EventBody::RunCompleted {
                run_id: run,
                disposition: RunDisposition::Completed { summary: None },
                chronicle: artifact,
            },
            EventBody::RunUsage {
                run_id: run,
                prompt_tokens: Some(1),
                completion_tokens: Some(2),
                cost_micros: None,
                coverage: None,
            },
            EventBody::ContextUsage {
                run_id: run,
                used_tokens: 100,
                window_tokens: 1000,
                system_tokens: 10,
                tool_tokens: 20,
                transcript_tokens: 70,
            },
            EventBody::LearningsCaptured {
                run_id: run,
                proposed_count: 1,
                proposed_ids: Vec::new(),
                activated_count: 0,
                activated_ids: Vec::new(),
            },
        ]
    }

    /// The invariant this function's doc states: a body carrying a `run_id` is
    /// run-scoped, so it must resolve to that run. `RunUsage` and
    /// `LearningsCaptured` were each journaled under a run id by the daemon
    /// while this copy still answered `None` for them.
    #[test]
    fn every_run_scoped_body_resolves_to_its_run() {
        let run = RunId::new();
        for body in run_scoped_bodies(run) {
            assert_eq!(
                event_run_id(&body),
                Some(run),
                "run-scoped body answered None: {body:?}"
            );
        }
    }

    /// The other half: a session-scoped body must stay `None`, or
    /// `stream_until_terminal` would attribute it to whatever run is live.
    #[test]
    fn a_session_scoped_body_belongs_to_no_run() {
        assert_eq!(
            event_run_id(&EventBody::SessionCreated {
                title: "t".to_owned()
            }),
            None
        );
        assert_eq!(event_run_id(&EventBody::SessionClosed), None);
    }

    async fn capture_text(run: RunId, bodies: Vec<EventBody>) -> (String, String, RunExit) {
        use codypendent_protocol::{write_envelope, Actor};
        use tokio::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            for (index, body) in bodies.into_iter().enumerate() {
                let event = SessionEvent {
                    sequence: index as u64 + 1,
                    occurred_at: chrono::Utc::now(),
                    causation_id: None,
                    correlation_id: None,
                    actor: Actor::System,
                    body,
                };
                write_envelope(
                    &mut socket,
                    &Envelope::request(ClientId::new(), Payload::Event(event)),
                )
                .await
                .unwrap();
            }
        });
        let mut connection = Connection::connect(&path).await.unwrap();
        let mut answer = Vec::new();
        let mut diagnostics = Vec::new();
        let exit = stream_until_terminal_with_writers(
            &mut connection,
            &mut answer,
            &mut diagnostics,
            Some(run),
            OutputFormat::Text,
        )
        .await
        .unwrap();
        server.await.unwrap();
        (
            String::from_utf8(answer).unwrap(),
            String::from_utf8(diagnostics).unwrap(),
            exit,
        )
    }

    fn completion(run_id: RunId, disposition: RunDisposition) -> EventBody {
        EventBody::RunCompleted {
            run_id,
            disposition,
            chronicle: ArtifactRef {
                id: ArtifactId::new(),
                media_type: "text/markdown".into(),
                byte_length: 0,
                sha256: "a".repeat(64),
                sensitivity: DataClassification::Internal,
            },
        }
    }

    #[tokio::test]
    async fn readable_usage_keeps_stdout_clean_and_qualifies_measured_subtotals() {
        let run = RunId::new();
        let (answer, diagnostics, exit) = capture_text(
            run,
            vec![
                EventBody::ModelStreamDelta {
                    run_id: run,
                    text: "The answer".into(),
                    thought: false,
                },
                EventBody::RunUsage {
                    run_id: run,
                    prompt_tokens: Some(1200),
                    completion_tokens: None,
                    cost_micros: Some(11),
                    coverage: Some(codypendent_protocol::UsageCoverage {
                        requests: 2,
                        prompt_requests: 1,
                        completion_requests: 0,
                        cost_requests: 1,
                        unmeasured_retries: 0,
                    }),
                },
                completion(run, RunDisposition::Completed { summary: None }),
            ],
        )
        .await;
        assert_eq!(answer, "The answer\n");
        assert_eq!(
            diagnostics,
            "codypendent: measured usage: 1,200 in · $0.000011 · partial\n"
        );
        assert_eq!(exit, RunExit::Completed);
    }

    #[tokio::test]
    async fn readable_legacy_usage_is_drained_without_admitting_sibling_output() {
        let run = RunId::new();
        let sibling = RunId::new();
        let (answer, diagnostics, exit) = capture_text(
            run,
            vec![
                completion(
                    run,
                    RunDisposition::Completed {
                        summary: Some("Done".into()),
                    },
                ),
                EventBody::ModelStreamDelta {
                    run_id: sibling,
                    text: "sibling answer".into(),
                    thought: false,
                },
                EventBody::RunUsage {
                    run_id: sibling,
                    prompt_tokens: Some(999),
                    completion_tokens: None,
                    cost_micros: None,
                    coverage: None,
                },
                EventBody::RunUsage {
                    run_id: run,
                    prompt_tokens: Some(0),
                    completion_tokens: None,
                    cost_micros: None,
                    coverage: None,
                },
            ],
        )
        .await;
        assert_eq!(answer, "Done\n");
        assert_eq!(
            diagnostics,
            "codypendent: measured usage: 0 in · coverage unknown\n"
        );
        assert_eq!(exit, RunExit::Completed);
    }

    #[tokio::test]
    async fn unmeasured_readable_failure_has_no_fabricated_cost_and_keeps_its_verdict() {
        let run = RunId::new();
        let (answer, diagnostics, exit) = capture_text(
            run,
            vec![
                EventBody::RunUsage {
                    run_id: run,
                    prompt_tokens: None,
                    completion_tokens: None,
                    cost_micros: None,
                    coverage: None,
                },
                completion(
                    run,
                    RunDisposition::Failed {
                        reason: "provider unavailable".into(),
                        error: None,
                    },
                ),
            ],
        )
        .await;
        assert!(answer.is_empty());
        assert_eq!(
            diagnostics,
            "codypendent: run failed — provider unavailable\n"
        );
        assert_eq!(exit, RunExit::Failed);
    }
}
