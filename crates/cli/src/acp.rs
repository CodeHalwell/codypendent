//! `codypendent acp` — expose the daemon as a Zed ACP agent (Phase 3 STEP 3.6).
//!
//! Zed launches `codypendent acp` and speaks the Agent Client Protocol over the
//! process's stdio. This module is the thin bridge: a [`DaemonAcpBackend`] that
//! turns ACP calls into daemon commands and daemon events back into ACP updates,
//! driven by the transport-agnostic server in
//! [`codypendent_integrations::acp`]. An ACP prompt starts a run; the run's
//! events stream back as `session/update`s; a tool that needs approval surfaces
//! as an ACP permission request; the client's answer resolves the approval.

use std::path::PathBuf;

use async_trait::async_trait;
use codypendent_integrations::acp::{
    serve as acp_serve, AcpBackend, AcpError, PermissionOption, PermissionOutcome, PromptSink,
    StopReason,
};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    AgentMode, ApprovalDecision, ApprovalScope, ClientRole, CommandBody, EventBody, Payload,
    ProposedAction, RunDisposition, RunId, SessionId, Subscription, WorkspaceId,
};
use serde_json::json;

use crate::connection::Connection;

/// Run the ACP server on this process's stdio until the client disconnects.
pub async fn serve(paths: &RuntimePaths, repo: PathBuf) -> anyhow::Result<()> {
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
        conn.handshake("codypendent-acp", env!("CARGO_PKG_VERSION"))
            .await
            .map_err(|e| AcpError::Backend(e.to_string()))?;
        Ok(conn)
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
        let reply = conn
            .send_command(CommandBody::CreateSession {
                workspace: WorkspaceId::new(),
                title: "acp".to_string(),
            })
            .await
            .map_err(|e| AcpError::Backend(e.to_string()))?;
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
        conn.send_command(CommandBody::AttachSession {
            session_id: session,
            last_seen_sequence: None,
            subscriptions: vec![Subscription::SessionSummary],
            requested_role: ClientRole::Contributor,
        })
        .await
        .map_err(|e| AcpError::Backend(e.to_string()))?;
        conn.send_command(CommandBody::StartRun {
            session_id: session,
            objective: text.to_string(),
            mode: AgentMode::Build,
            repository: Some(self.repo.to_string_lossy().into_owned()),
        })
        .await
        .map_err(|e| AcpError::Backend(e.to_string()))?;

        let mut run_id: Option<RunId> = None;
        loop {
            tokio::select! {
                // The client cancelled this turn: stop the run and report it.
                _ = ctx.cancelled() => {
                    if let Some(run) = run_id {
                        let _ = conn.send_command(CommandBody::CancelRun { run_id: run }).await;
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
                    match event.body {
                        EventBody::RunStarted { run_id: run, .. } => run_id = Some(run),
                        EventBody::ModelStreamDelta { text, .. } => {
                            ctx.update(json!({ "type": "agent_text", "text": text })).await;
                        }
                        EventBody::ToolStarted { tool, .. } => {
                            ctx.update(json!({ "type": "tool", "tool": tool })).await;
                        }
                        EventBody::NoteAppended { text, .. } => {
                            ctx.update(json!({ "type": "note", "text": text })).await;
                        }
                        EventBody::ToolProposed { approval_id, action, .. } => {
                            resolve(&mut conn, ctx, approval_id, &action).await?;
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

/// Surface a pending approval as an ACP permission request and resolve it with
/// the client's answer.
async fn resolve(
    conn: &mut Connection,
    ctx: &mut dyn PromptSink,
    approval_id: codypendent_protocol::ApprovalId,
    action: &ProposedAction,
) -> Result<(), AcpError> {
    let outcome = ctx
        .request_permission(json!({ "action": action }), permission_options())
        .await;
    let decision = match outcome {
        PermissionOutcome::Selected(id) if id == "allow" => ApprovalDecision::Approve,
        _ => ApprovalDecision::Reject,
    };
    conn.send_command(CommandBody::ResolveApproval {
        approval_id,
        decision,
        scope: ApprovalScope::Once,
    })
    .await
    .map_err(|e| AcpError::Backend(e.to_string()))?;
    Ok(())
}
