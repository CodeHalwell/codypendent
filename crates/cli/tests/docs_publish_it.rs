//! Phase 4 STEP 4.4: the `codypendent docs publish` client core.
//!
//! Like `workflow_it.rs`, this drives `codypendent_cli::commands`' connected
//! core (`docs_publish_over_connection`) against a hand-rolled mock daemon
//! built only from `codypendent_protocol`'s framing — no `codypendentd`
//! subprocess — asserting that the client handshakes, binds the `Controller`
//! role, sends `PublishDocument` with the target verbatim, and then resolves
//! the parked approval with the decision it was given.

use std::time::Duration;

use codypendent_cli::commands::docs_publish_over_connection;
use codypendent_cli::connection::Connection;
use codypendent_protocol::document::PublishTarget;
use codypendent_protocol::{
    read_envelope, write_envelope, ApprovalDecision, ApprovalId, ApprovalScope, CodypendentError,
    Command, CommandBody, CommandId, DaemonInstanceId, DocumentId, Envelope, Payload, ServerHello,
    PROTOCOL_V1,
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

/// Play the daemon's side of one `docs publish`: `ClientHello` ->
/// `ServerHello`; `AttachSession` (the role bind) -> the `session-not-found`
/// rejection the client ignores; `PublishDocument` -> `DocumentPublishRequested`
/// with a scripted approval id; `ResolveApproval` -> `CommandAccepted`.
async fn mock_daemon(
    mut stream: UnixStream,
    document_id: DocumentId,
    approval_id: ApprovalId,
    expected_decision: ApprovalDecision,
) {
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

    // 2. AttachSession binds the Controller role; the throwaway session does
    // not exist, so the real daemon rejects it session-not-found — the client
    // ignores the reply (the role has already bound), exactly as `workflow
    // run` does.
    let attach = read_envelope(&mut stream)
        .await
        .expect("read AttachSession")
        .expect("connection open");
    match &expect_command(&attach).body {
        CommandBody::AttachSession { requested_role, .. } => {
            assert_eq!(
                *requested_role,
                codypendent_protocol::ClientRole::Controller,
                "the client must bind Controller to publish a document"
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

    // 3. PublishDocument -> DocumentPublishRequested with a scripted approval id.
    let publish = read_envelope(&mut stream)
        .await
        .expect("read PublishDocument")
        .expect("connection open");
    let command_id = command_id_of(&publish);
    match &expect_command(&publish).body {
        CommandBody::PublishDocument {
            document_id: sent_id,
            target,
        } => {
            assert_eq!(*sent_id, document_id);
            assert_eq!(
                *target,
                PublishTarget::RepositoryFile {
                    path: "docs/architecture.md".to_string(),
                }
            );
        }
        other => panic!("expected PublishDocument, got {other:?}"),
    }
    write_envelope(
        &mut stream,
        &Envelope::reply_to(
            &publish,
            Payload::DocumentPublishRequested {
                command_id,
                approval_id,
                target: "repository file docs/architecture.md".to_string(),
                changed_files: vec!["docs/architecture.md".to_string()],
                git_action: "write docs/architecture.md in the working tree \
                             (approval-gated change set)"
                    .to_string(),
            },
        ),
    )
    .await
    .expect("write DocumentPublishRequested");

    // 4. ResolveApproval -> CommandAccepted, carrying exactly the decision the
    // client was given (approve when the operator confirmed, reject otherwise).
    let resolve = read_envelope(&mut stream)
        .await
        .expect("read ResolveApproval")
        .expect("connection open");
    let command_id = command_id_of(&resolve);
    match &expect_command(&resolve).body {
        CommandBody::ResolveApproval {
            approval_id: sent_id,
            decision,
            scope,
        } => {
            assert_eq!(*sent_id, approval_id);
            assert_eq!(*decision, expected_decision);
            assert_eq!(*scope, ApprovalScope::Once);
        }
        other => panic!("expected ResolveApproval, got {other:?}"),
    }
    write_envelope(
        &mut stream,
        &Envelope::reply_to(
            &resolve,
            Payload::CommandAccepted {
                command_id,
                sequence: None,
                created_run: None,
            },
        ),
    )
    .await
    .expect("write CommandAccepted");
}

#[tokio::test]
async fn approved_publish_sends_the_target_verbatim_and_resolves_approve() {
    let document_id = DocumentId::new();
    let approval_id = ApprovalId::new();
    let (socket, listener) = MockSocket::bind();
    let server = tokio::spawn(async move {
        let (stream, _addr) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("mock accepted a connection in time")
            .expect("accept");
        mock_daemon(stream, document_id, approval_id, ApprovalDecision::Approve).await;
    });

    let mut conn = Connection::connect(&socket.path).await.expect("connect");
    let target = PublishTarget::RepositoryFile {
        path: "docs/architecture.md".to_string(),
    };
    let returned =
        docs_publish_over_connection(&mut conn, document_id, target, ApprovalDecision::Approve)
            .await
            .expect("docs publish");
    assert_eq!(returned, approval_id);

    server.await.expect("mock server task");
}

#[tokio::test]
async fn rejected_publish_resolves_reject() {
    let document_id = DocumentId::new();
    let approval_id = ApprovalId::new();
    let (socket, listener) = MockSocket::bind();
    let server = tokio::spawn(async move {
        let (stream, _addr) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("mock accepted a connection in time")
            .expect("accept");
        mock_daemon(stream, document_id, approval_id, ApprovalDecision::Reject).await;
    });

    let mut conn = Connection::connect(&socket.path).await.expect("connect");
    let target = PublishTarget::RepositoryFile {
        path: "docs/architecture.md".to_string(),
    };
    let returned =
        docs_publish_over_connection(&mut conn, document_id, target, ApprovalDecision::Reject)
            .await
            .expect("docs publish");
    assert_eq!(returned, approval_id);

    server.await.expect("mock server task");
}
