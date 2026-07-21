//! Phase 5 STEP 5.2: the `codypendent workflow run` client core.
//!
//! Like `jsonl_it.rs`, this drives `codypendent_cli::commands`' connected core
//! (`workflow_run_over_connection`) against a hand-rolled mock daemon built only
//! from `codypendent_protocol`'s framing — no `codypendentd` subprocess — asserting
//! that the client handshakes, binds the `Controller` role, sends `StartWorkflow`,
//! and returns the run id the daemon reports.

use std::time::Duration;

use codypendent_cli::commands::workflow_run_over_connection;
use codypendent_cli::connection::Connection;
use codypendent_protocol::{
    read_envelope, write_envelope, CodypendentError, Command, CommandBody, CommandId,
    DaemonInstanceId, Envelope, Payload, ServerHello, PROTOCOL_V1,
};
use tokio::net::{UnixListener, UnixStream};

struct MockSocket {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

impl MockSocket {
    fn bind() -> (Self, UnixListener) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("d.sock");
        let listener = UnixListener::bind(&path).expect("bind mock socket");
        (Self { _dir: dir, path }, listener)
    }
}

fn command_id_of(request: &Envelope) -> CommandId {
    match &request.payload {
        Payload::Command(command) => command.command_id,
        other => panic!("expected a Command envelope, got {other:?}"),
    }
}

fn expect_command(request: &Envelope) -> &Command {
    match &request.payload {
        Payload::Command(command) => command,
        other => panic!("expected a Command envelope, got {other:?}"),
    }
}

/// Play the daemon's side of one `workflow run`: `ClientHello` -> `ServerHello`;
/// `AttachSession` (the role bind) -> the `session-not-found` rejection the client
/// ignores; `StartWorkflow` -> `WorkflowRunStarted` with a scripted run id.
async fn mock_daemon(mut stream: UnixStream, workflow_run_id: &str) {
    // 1. Handshake.
    let hello = read_envelope(&mut stream)
        .await
        .expect("read ClientHello")
        .expect("connection open");
    assert!(matches!(hello.payload, Payload::ClientHello(_)));
    write_envelope(
        &mut stream,
        &Envelope::reply_to(
            &hello,
            Payload::ServerHello(ServerHello {
                resume_token: None,
                selected_protocol: PROTOCOL_V1,
                daemon_version: "mock".to_string(),
                daemon_instance: DaemonInstanceId::new(),
                heartbeat_interval_ms: 15_000,
            }),
        ),
    )
    .await
    .expect("write ServerHello");

    // 2. AttachSession binds the Controller role; the throwaway session does not
    // exist, so the real daemon rejects it session-not-found — the client ignores
    // the reply (the role has already bound).
    let attach = read_envelope(&mut stream)
        .await
        .expect("read AttachSession")
        .expect("connection open");
    match &expect_command(&attach).body {
        CommandBody::AttachSession { requested_role, .. } => {
            assert_eq!(
                *requested_role,
                codypendent_protocol::ClientRole::Controller,
                "the client must bind Controller to start a workflow"
            );
        }
        other => panic!("expected AttachSession, got {other:?}"),
    }
    write_envelope(
        &mut stream,
        &Envelope::reply_to(
            &attach,
            Payload::CommandRejected(CodypendentError::new(
                "protocol.session-not-found",
                "unknown session",
                false,
            )),
        ),
    )
    .await
    .expect("write attach rejection");

    // 3. StartWorkflow -> WorkflowRunStarted with the scripted run id.
    let start = read_envelope(&mut stream)
        .await
        .expect("read StartWorkflow")
        .expect("connection open");
    let command_id = command_id_of(&start);
    match &expect_command(&start).body {
        CommandBody::StartWorkflow {
            manifest,
            repository,
            ..
        } => {
            assert!(
                manifest.contains("schema_version"),
                "the manifest content crosses the wire, not a path"
            );
            assert_eq!(
                repository.as_deref(),
                Some("/tmp/workflow-repo"),
                "the run's repository crosses the wire (Phase 5 T5)"
            );
        }
        other => panic!("expected StartWorkflow, got {other:?}"),
    }
    write_envelope(
        &mut stream,
        &Envelope::reply_to(
            &start,
            Payload::WorkflowRunStarted {
                command_id,
                workflow_run_id: workflow_run_id.to_string(),
            },
        ),
    )
    .await
    .expect("write WorkflowRunStarted");
}

#[tokio::test]
async fn workflow_run_sends_the_manifest_and_returns_the_run_id() {
    let (socket, listener) = MockSocket::bind();
    let server = tokio::spawn(async move {
        let (stream, _addr) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("mock accepted a connection in time")
            .expect("accept");
        mock_daemon(stream, "wfrun-test-123").await;
    });

    let mut conn = Connection::connect(&socket.path).await.expect("connect");
    // A bare-but-valid manifest; the mock only checks the content crossed the wire.
    let manifest =
        "schema_version: 1\nid: wf\nversion: 1\nsteps:\n  - id: a\n    tool: repository.test\n"
            .to_string();
    let run_id = workflow_run_over_connection(
        &mut conn,
        manifest,
        serde_json::json!({ "pr": 7 }),
        Some("/tmp/workflow-repo".to_string()),
    )
    .await
    .expect("workflow run");
    assert_eq!(run_id, "wfrun-test-123");

    server.await.expect("mock server task");
}
