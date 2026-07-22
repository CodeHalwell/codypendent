//! JSONL rendering of the session event stream — shared by
//! `codypendent run --jsonl` and `codypendent attach --events jsonl` (STEP
//! 1.13). Not reused by the TUI (STEP 1.12): the TUI will consume
//! `connection::Connection` envelopes directly and render them as widgets,
//! whereas this module's only job is "one self-describing JSON `Envelope` per
//! stdout line" — the JSONL stream and the TUI observe the same events, never
//! a privileged side channel.

use std::io::Write;

use anyhow::{anyhow, Context};
use codypendent_protocol::{
    Catchup, ClientId, Envelope, EventBody, MessageId, Payload, RunDisposition, RunId, RunState,
    SessionEvent, SessionId, PROTOCOL_V1,
};

use crate::connection::Connection;

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
/// individual events, so it produces no JSONL lines — the caller's live
/// stream simply continues from `through`. A future `Catchup::Unknown`
/// variant is likewise skipped rather than failing the whole attach (RULE 1).
pub fn replay_catchup<W: Write>(
    out: &mut W,
    client_id: ClientId,
    session_id: SessionId,
    catchup: Catchup,
) -> anyhow::Result<()> {
    if let Catchup::Events { events, .. } = catchup {
        for event in events {
            write_line(out, &envelope_for(client_id, session_id, event))?;
        }
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
pub async fn stream_until_terminal<W: Write>(
    conn: &mut Connection,
    out: &mut W,
    expected_run: Option<RunId>,
) -> anyhow::Result<RunExit> {
    // The authoritative binding is the run id the daemon reported for OUR
    // StartRun; first-observed `RunStarted` is only the older-daemon fallback.
    let mut run_id: Option<RunId> = expected_run;
    loop {
        let envelope = conn.next_envelope().await?.ok_or_else(|| {
            anyhow!("daemon closed the connection before the run reached a terminal state")
        })?;
        let Payload::Event(event) = &envelope.payload else {
            continue; // not an Event payload (e.g. a stray reply); ignore
        };
        if let EventBody::RunStarted { run_id: rid, .. } = &event.body {
            run_id.get_or_insert(*rid);
        }

        write_line(out, &envelope)?;

        let owns_event = matches!(event_run_id(&event.body), Some(rid) if Some(rid) == run_id);
        if !owns_event {
            continue;
        }
        let exit = match &event.body {
            EventBody::RunCompleted { disposition, .. } => RunExit::from_disposition(disposition),
            EventBody::RunStateChanged { state, .. } => RunExit::from_state(*state),
            _ => None,
        };
        if let Some(exit) = exit {
            return Ok(exit);
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
pub(crate) fn event_run_id(body: &EventBody) -> Option<RunId> {
    match body {
        EventBody::RunStarted { run_id, .. }
        | EventBody::RunStateChanged { run_id, .. }
        | EventBody::ModelStreamDelta { run_id, .. }
        | EventBody::ToolProposed { run_id, .. }
        | EventBody::ToolStarted { run_id, .. }
        | EventBody::ToolCompleted { run_id, .. }
        | EventBody::PatchProposed { run_id, .. }
        | EventBody::SteeringQueued { run_id }
        | EventBody::SteeringApplied { run_id }
        | EventBody::BudgetWarning { run_id, .. }
        | EventBody::RunCompleted { run_id, .. } => Some(*run_id),
        _ => None,
    }
}
