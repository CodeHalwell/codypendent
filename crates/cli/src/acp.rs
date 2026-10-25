//! `codypendent acp` — expose the daemon as a Zed ACP agent (Phase 3 STEP 3.6).
//!
//! Zed launches `codypendent acp` and speaks the Agent Client Protocol over the
//! process's stdio. This module is the thin bridge: a [`DaemonAcpBackend`] that
//! turns ACP calls into daemon commands and daemon events back into ACP updates,
//! driven by the transport-agnostic server in
//! [`codypendent_integrations::acp`]. An ACP prompt starts a run; the run's
//! events stream back as `session/update`s; a tool that needs approval surfaces
//! as an ACP permission request; the client's answer resolves the approval.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::connection::Connection;
use crate::stream::event_run_id;
use async_trait::async_trait;
use codypendent_integrations::acp::{
    agent_message_chunk, agent_thought_chunk, permission_tool_call, serve as acp_serve,
    tool_call_completed, tool_call_pending, tool_call_refined, tool_call_started, AcpBackend,
    AcpError, PermissionOption, PermissionOutcome, PromptSink, StopReason,
};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    AgentMode, ApprovalDecision, ApprovalScope, ClientRole, CommandBody, EventBody, Payload,
    ProposedAction, RunDisposition, RunId, SessionId, Subscription, WorkspaceId,
};

/// Run the ACP server on this process's stdio until the client disconnects.
pub async fn serve(paths: &RuntimePaths, repo: PathBuf) -> anyhow::Result<()> {
    // Ensure the daemon BEFORE the stdio loop starts: `open()` only connects
    // on the first prompt, so without this a Zed user saw the raw socket
    // errno inside their chat, minutes after launching. Failure here still
    // exits on stderr, where a launcher can show it.
    crate::commands::ensure_daemon(paths).await?;
    let backend = DaemonAcpBackend {
        socket: paths.socket_path.clone(),
        repo,
    };
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::stdout();
    acp_serve(reader, writer, backend)
        .await
        .map_err(|error| anyhow::anyhow!("acp server: {error}"))
}

/// An [`AcpBackend`] backed by a running daemon over its Unix socket.
struct DaemonAcpBackend {
    socket: PathBuf,
    repo: PathBuf,
}

impl DaemonAcpBackend {
    /// Open a handshaken connection to the daemon.
    async fn open(&self) -> Result<Connection, AcpError> {
        let mut conn = Connection::connect(&self.socket)
            .await
            .map_err(|e| AcpError::Backend(e.to_string()))?;
        conn.handshake("codypendent-acp", env!("CARGO_PKG_VERSION"), None)
            .await
            .map_err(|e| AcpError::Backend(e.to_string()))?;
        Ok(conn)
    }
}

fn require_accepted(
    reply: codypendent_protocol::Envelope,
    command: &str,
) -> Result<codypendent_protocol::Envelope, AcpError> {
    match &reply.payload {
        Payload::CommandAccepted { .. } => Ok(reply),
        Payload::CommandRejected(error) => Err(AcpError::Backend(format!(
            "{command} rejected: {} ({})",
            error.message, error.code
        ))),
        other => Err(AcpError::Backend(format!(
            "unexpected reply to {command}: {other:?}"
        ))),
    }
}

/// `AttachSession` is answered with the session's catch-up snapshot, never with
/// `CommandAccepted` — it is served ahead of the command ledger like the other
/// connection-level commands (`server::handle_attach`). Checking it with
/// [`require_accepted`] failed every attach, and therefore every `session/prompt`
/// this backend served. `commands::expect_catchup` is the same contract for the
/// `run`/`attach` clients.
fn require_attached(reply: codypendent_protocol::Envelope) -> Result<(), AcpError> {
    match &reply.payload {
        Payload::Catchup { .. } => Ok(()),
        Payload::CommandRejected(error) => Err(AcpError::Backend(format!(
            "AttachSession rejected: {} ({})",
            error.message, error.code
        ))),
        other => Err(AcpError::Backend(format!(
            "unexpected reply to AttachSession: {other:?}"
        ))),
    }
}

/// The two options every permission request offers.
fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption {
            option_id: "allow".to_string(),
            name: "Approve".to_string(),
            kind: "allow_once".to_string(),
        },
        PermissionOption {
            option_id: "reject".to_string(),
            name: "Reject".to_string(),
            kind: "reject_once".to_string(),
        },
    ]
}

#[async_trait]
impl AcpBackend for DaemonAcpBackend {
    async fn new_session(&self) -> Result<String, AcpError> {
        let mut conn = self.open().await?;
        let reply = require_accepted(
            conn.send_command(CommandBody::CreateSession {
                workspace: WorkspaceId::new(),
                title: "acp".to_string(),
                // So the daemon can build its code graph on open, not only on
                // the first run (mirrors `StartRun.repository` below).
                repository: Some(self.repo.to_string_lossy().into_owned()),
                internal: false,
                parent_session_id: None,
                parent_run_id: None,
            })
            .await
            .map_err(|e| AcpError::Backend(e.to_string()))?,
            "CreateSession",
        )?;
        let session = reply
            .session_id
            .ok_or_else(|| AcpError::Backend("daemon returned no session id".to_string()))?;
        Ok(session.to_string())
    }

    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        ctx: &mut dyn PromptSink,
    ) -> Result<StopReason, AcpError> {
        let session: SessionId = session_id
            .parse()
            .map_err(|e| AcpError::Backend(format!("bad session id: {e}")))?;
        let mut conn = self.open().await?;

        // Attach so this connection receives the run's events, then start the
        // run with the prompt as its objective.
        require_attached(
            conn.send_command(CommandBody::AttachSession {
                session_id: session,
                last_seen_sequence: None,
                subscriptions: vec![Subscription::SessionSummary],
                // Approver, not Contributor: this connection must both start runs AND
                // resolve the approvals it surfaces as ACP permission requests. The
                // daemon gates `ResolveApproval` to Approver/Controller, and Approver
                // is a superset of Contributor's start/submit permissions.
                requested_role: ClientRole::Approver,
                repository: Some(self.repo.to_string_lossy().into_owned()),
            })
            .await
            .map_err(|e| AcpError::Backend(e.to_string()))?,
        )?;
        let start_reply = require_accepted(
            conn.send_command(CommandBody::StartRun {
                session_id: session,
                objective: text.to_string(),
                mode: AgentMode::Build,
                repository: Some(self.repo.to_string_lossy().into_owned()),
                // The ACP bridge pins no model; the daemon resolves/routes as usual.
                model: None,
            })
            .await
            .map_err(|e| AcpError::Backend(e.to_string()))?,
            "StartRun",
        )?;

        // The authoritative binding is the run id the daemon reported for OUR
        // `StartRun`. Everything below is filtered to it: a session can carry
        // more than one run at a time (another client's `StartRun`, a queued
        // prompt draining, a workflow node), and a turn that adopts whichever
        // run it hears from next reports that run's output as this turn's,
        // ends this turn on that run's `RunCompleted`, and — worst — cancels
        // ANOTHER CLIENT'S RUN when Zed cancels this one. Same binding rule as
        // the JSONL streamer's `owns_event` (`crate::stream`).
        let mut run_id: Option<RunId> = match start_reply.payload {
            Payload::CommandAccepted { created_run, .. } => created_run,
            _ => unreachable!("require_accepted checked payload"),
        };
        // Mints the `toolCallId`s this turn's ACP tool calls are referred to by
        // (see the `ToolStarted` arm).
        let mut tool_calls = 0_u64;
        let mut tool_ids: BTreeMap<String, Vec<String>> = BTreeMap::new();
        loop {
            tokio::select! {
                // The client cancelled this turn: stop the run and report it.
                // `next_envelope` is NOT cancellation-safe — if this arm wins
                // mid-frame, `conn`'s read stream is desynchronized (the
                // consumed prefix bytes are gone), so the cancel must never
                // reuse it: a CancelRun written onto a desynced connection
                // fails silently and the run keeps executing while Zed shows
                // "cancelled". Open a fresh connection for the cancel instead.
                _ = ctx.cancelled() => {
                    if let Some(run) = run_id {
                        match self.open().await {
                            Ok(mut fresh) => {
                                let cancel = fresh
                                    .send_command(CommandBody::CancelRun { run_id: run })
                                    .await
                                    .map_err(|error| AcpError::Backend(error.to_string()))
                                    .and_then(|reply| require_accepted(reply, "CancelRun"));
                                if let Err(error) = cancel {
                                    eprintln!("codypendent acp: cancel could not reach the daemon: {error}");
                                }
                            }
                            Err(error) => {
                                eprintln!("codypendent acp: cancel could not reconnect to the daemon: {error}");
                            }
                        }
                    }
                    return Ok(StopReason::Cancelled);
                }
                envelope = conn.next_envelope() => {
                    let envelope = envelope.map_err(|e| AcpError::Backend(e.to_string()))?;
                    let Some(envelope) = envelope else {
                        // Daemon closed the connection: treat as end of turn.
                        return Ok(StopReason::EndTurn);
                    };
                    let Payload::Event(event) = envelope.payload else { continue };
                    // Bind BEFORE filtering, and only ever `get_or_insert`: a
                    // `RunStarted` is the fallback binding for a daemon whose
                    // `CommandAccepted` carried no `created_run`, never a
                    // rebinding of a turn that already knows its run.
                    if let EventBody::RunStarted { run_id: started, .. } = &event.body {
                        run_id.get_or_insert(*started);
                    }
                    // A run-scoped event from a DIFFERENT run is not this
                    // turn's; it is dropped before it can stream output, mint a
                    // tool call, or end the turn.
                    if !belongs_to_turn(&event.body, run_id) {
                        continue;
                    }
                    match event.body {
                        EventBody::ModelStreamDelta { text, .. } => {
                            ctx.update(agent_message_chunk(text)).await;
                        }
                        EventBody::ToolStarted { tool, label, .. } => {
                            // ACP requires a `toolCallId` on every tool call.
                            // The daemon event carries none (its tools are
                            // named, not id'd), so the bridge mints the id this
                            // ACP session uses to refer to the call.
                            tool_calls += 1;
                            let id = format!("tool-{tool_calls}");
                            let title = if tool.starts_with("acp:") {
                                label.unwrap_or_else(|| "Tool".to_owned())
                            } else { tool.clone() };
                            tool_ids.entry(tool).or_default().push(id.clone());
                            ctx.update(tool_call_started(id, title)).await;
                        }
                        EventBody::ToolUpdated { tool, label, .. } => {
                            if let (Some(id), Some(label)) = (tool_ids.get(&tool).and_then(|ids| ids.last()), label) {
                                ctx.update(tool_call_refined(id.clone(), label)).await;
                            }
                        }
                        EventBody::ToolCompleted { tool, outcome, .. } => {
                            if let Some(id) = tool_ids.get_mut(&tool).and_then(Vec::pop) {
                                ctx.update(tool_call_completed(id, &outcome)).await;
                            }
                        }
                        // A run note is host commentary, not model output, so it
                        // must not arrive as an agent message chunk.
                        EventBody::NoteAppended { text, .. } => {
                            ctx.update(agent_thought_chunk(text)).await;
                        }
                        // A single approval surfaces as BOTH `ApprovalRequested`
                        // (from the broker) and `ToolProposed` (from the runtime).
                        // Prompt on exactly one — `ApprovalRequested` — so the
                        // client sees one permission request and we send one
                        // `ResolveApproval`; `ToolProposed` is display-only.
                        EventBody::ToolProposed { run_id: _, action, .. } => {
                            tool_calls += 1;
                            ctx.update(tool_call_pending(
                                format!("tool-{tool_calls}"),
                                action_title(&action),
                            ))
                            .await;
                        }
                        EventBody::ApprovalRequested { approval_id, action, .. } => {
                            resolve(&mut conn, ctx, approval_id, &action).await?;
                        }
                        EventBody::RunCompleted { disposition, .. } => {
                            return Ok(match disposition {
                                RunDisposition::Cancelled { .. } => StopReason::Cancelled,
                                _ => StopReason::EndTurn,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Whether `body` belongs to the turn bound to `run_id` — the ACP bridge's
/// copy of the JSONL streamer's `owns_event` rule (`crate::stream`).
///
/// A session carries more than one run: another client's `StartRun`, a queued
/// prompt draining, a workflow node. Every one of their events reaches this
/// connection (a client sees everything it is subscribed to), and a turn that
/// acts on them streams another run's tokens as its own, ends on another run's
/// `RunCompleted`, and cancels another run when Zed cancels this turn.
///
/// Events that carry no run of their own are session-scoped and still belong to
/// the turn in focus — `ApprovalRequested` above all, which this bridge is the
/// session's approver for: dropping it would leave the approval unanswered and
/// the run parked forever. A `NoteAppended` is the one event with an OPTIONAL
/// run: `Some` is run-scoped and filtered, `None` is session-level and kept.
fn belongs_to_turn(body: &EventBody, run_id: Option<RunId>) -> bool {
    match body {
        EventBody::NoteAppended { run_id: note, .. } => note.is_none() || *note == run_id,
        other => match event_run_id(other) {
            Some(event_run) => Some(event_run) == run_id,
            None => true,
        },
    }
}

/// A short, human-readable title for the action a permission request covers.
/// Derived from the action's own typed fields — never invented, and never the
/// raw serialized action, which a client renders as opaque JSON. A variant this
/// build does not know (`ProposedAction` is `#[non_exhaustive]`) still gets an
/// honest generic title rather than failing to compile or panicking.
fn action_title(action: &ProposedAction) -> String {
    match action {
        ProposedAction::ReadFiles { paths } => format!("read {} file(s)", paths.len()),
        ProposedAction::WritePatch { .. } => "apply a patch".to_string(),
        ProposedAction::ExecuteCommand { program, .. } => format!("run {program}"),
        ProposedAction::NetworkRequest { destination } => {
            format!("network request to {destination}")
        }
        ProposedAction::GitCommit { repository } => format!("git commit in {repository}"),
        ProposedAction::GitPush { remote, branch } => format!("git push {branch} to {remote}"),
        ProposedAction::GitHubMutation { summary, .. } => summary.clone(),
        ProposedAction::PublishDocument { target, .. } => format!("publish to {target}"),
        ProposedAction::BlackboardPost { kind, .. } => format!("post {kind} to the blackboard"),
        ProposedAction::BlackboardQuery { .. } => "query the blackboard".to_string(),
        ProposedAction::McpToolCall { summary, .. } => summary.clone(),
        ProposedAction::CouncilCreate { summary, .. }
        | ProposedAction::CouncilRun { summary, .. }
        | ProposedAction::WorkflowCreate { summary, .. }
        | ProposedAction::WorkflowRun { summary, .. } => summary.clone(),
        _ => "tool call".to_string(),
    }
}

/// Surface a pending approval as an ACP permission request and resolve it with
/// the client's answer.
async fn resolve(
    conn: &mut Connection,
    ctx: &mut dyn PromptSink,
    approval_id: codypendent_protocol::ApprovalId,
    action: &ProposedAction,
) -> Result<(), AcpError> {
    // The approval id makes a stable, real `toolCallId` for the call this
    // request authorizes — the field a spec-strict client needs to correlate
    // the two, and which the previous `{"action": …}` payload lacked entirely.
    let outcome = ctx
        .request_permission(
            permission_tool_call(format!("approval-{approval_id}"), action_title(action)),
            permission_options(),
        )
        .await;
    let decision = match outcome {
        PermissionOutcome::Selected(id) if id == "allow" => ApprovalDecision::Approve,
        _ => ApprovalDecision::Reject,
    };
    let reply = conn
        .send_command(CommandBody::ResolveApproval {
            approval_id,
            decision,
            scope: ApprovalScope::Once,
        })
        .await
        .map_err(|e| AcpError::Backend(e.to_string()))?;
    require_accepted(reply, "ResolveApproval")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::{ApprovalId, Risk, RiskLevel, RunState};

    /// The turn is bound to the run the daemon reported for OUR `StartRun`.
    /// Before this, any `RunStarted` in the session rebound it and any
    /// `RunCompleted` ended it — so a concurrent run's completion ended this
    /// turn, its tokens were streamed as this turn's, and the cancel path fired
    /// `CancelRun` at ANOTHER CLIENT'S run. Dropping the ownership filter makes
    /// every `foreign` assertion below fail.
    #[test]
    fn only_the_turns_own_run_drives_the_turn() {
        let ours = RunId::new();
        let theirs = RunId::new();
        let bound = Some(ours);

        // Run-scoped events: ours are handled, another run's are dropped.
        for body in [
            EventBody::ModelStreamDelta {
                run_id: ours,
                text: "hi".to_string(),
                thought: false,
            },
            EventBody::RunCompleted {
                run_id: ours,
                disposition: RunDisposition::Completed { summary: None },
                chronicle: chronicle(),
            },
            EventBody::RunStateChanged {
                run_id: ours,
                state: RunState::Running,
            },
        ] {
            assert!(belongs_to_turn(&body, bound), "{body:?} is this turn's");
        }
        for body in [
            EventBody::ModelStreamDelta {
                run_id: theirs,
                text: "not mine".to_string(),
                thought: false,
            },
            EventBody::RunCompleted {
                run_id: theirs,
                disposition: RunDisposition::Completed { summary: None },
                chronicle: chronicle(),
            },
            EventBody::RunStarted {
                run_id: theirs,
                objective: "another client's run".to_string(),
                mode: AgentMode::Build,
            },
        ] {
            assert!(
                !belongs_to_turn(&body, bound),
                "{body:?} belongs to another run"
            );
        }

        // Session-scoped events have no run of their own and are still handled;
        // an `ApprovalRequested` dropped here would park a run forever.
        assert!(belongs_to_turn(
            &EventBody::ApprovalRequested {
                approval_id: ApprovalId::new(),
                action: ProposedAction::GitCommit {
                    repository: "acme/widget".to_string(),
                },
                risk: Risk {
                    level: RiskLevel::Medium,
                    reasons: vec![],
                },
                pattern: None,
            },
            bound
        ));

        // A note is the one event with an OPTIONAL run: session-level notes are
        // kept, another run's note is not.
        assert!(belongs_to_turn(
            &EventBody::NoteAppended {
                text: "session note".to_string(),
                run_id: None,
            },
            bound
        ));
        assert!(belongs_to_turn(
            &EventBody::NoteAppended {
                text: "our note".to_string(),
                run_id: Some(ours),
            },
            bound
        ));
        assert!(!belongs_to_turn(
            &EventBody::NoteAppended {
                text: "their note".to_string(),
                run_id: Some(theirs),
            },
            bound
        ));
    }

    fn chronicle() -> codypendent_protocol::ArtifactRef {
        codypendent_protocol::ArtifactRef {
            id: codypendent_protocol::ArtifactId::new(),
            media_type: "application/json".to_string(),
            byte_length: 2,
            sha256: "a".repeat(64),
            sensitivity: codypendent_protocol::DataClassification::Internal,
        }
    }
}
