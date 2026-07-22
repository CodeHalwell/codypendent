//! Phase 7 STEP 7.5: the `codypendent promote` client core.
//!
//! Like `workflow_it.rs`, this drives `codypendent_cli::commands`' connected
//! cores (`promote_propose_over_connection`, `promotion_command_over_connection`)
//! against a hand-rolled mock daemon built only from `codypendent_protocol`'s
//! framing — no `codypendentd` subprocess — asserting that the client
//! handshakes, binds the `Controller` role, sends the right `CommandBody`, and
//! handles both the accept and reject replies.

use std::time::Duration;

use codypendent_cli::commands::{
    promote_propose_over_connection, promotion_command_over_connection,
};
use codypendent_cli::connection::Connection;
use codypendent_protocol::{
    read_envelope, write_envelope, ClientRole, CodypendentError, Command, CommandBody, CommandId,
    DaemonInstanceId, Envelope, Payload, PromotionAction, ServerHello, PROTOCOL_V1,
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

/// Handshake, then bind the Controller role exactly like `workflow_it.rs`'s
/// mock: the client attaches to a throwaway session id purely to bind the
/// role, and the real daemon would reject it `session-not-found` — a rejection
/// this client ignores (the role has already bound before the reply arrives).
async fn handshake_and_bind(stream: &mut UnixStream) {
    let hello = read_envelope(stream).await.unwrap().unwrap();
    assert!(matches!(hello.payload, Payload::ClientHello(_)));
    write_envelope(
        stream,
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
    .unwrap();

    let attach = read_envelope(stream).await.unwrap().unwrap();
    match &expect_command(&attach).body {
        CommandBody::AttachSession { requested_role, .. } => {
            assert_eq!(
                *requested_role,
                ClientRole::Controller,
                "every promote subcommand binds Controller"
            );
        }
        other => panic!("expected AttachSession, got {other:?}"),
    }
    write_envelope(
        stream,
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
    .unwrap();
}

#[tokio::test]
async fn propose_sends_the_right_body_and_returns_the_candidate_id() {
    let (socket, listener) = MockSocket::bind();
    let server = tokio::spawn(async move {
        let (mut stream, _addr) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        handshake_and_bind(&mut stream).await;

        let propose = read_envelope(&mut stream).await.unwrap().unwrap();
        let command_id = command_id_of(&propose);
        match &expect_command(&propose).body {
            CommandBody::ProposePromotion {
                kind,
                name,
                version,
                requires_permission_review,
            } => {
                assert_eq!(kind, "router");
                assert_eq!(name, "tool-selection");
                assert_eq!(*version, 12);
                assert!(!requires_permission_review);
            }
            other => panic!("expected ProposePromotion, got {other:?}"),
        }
        write_envelope(
            &mut stream,
            &Envelope::reply_to(
                &propose,
                Payload::PromotionProposed {
                    command_id,
                    candidate_id: "cand-test-1".to_string(),
                },
            ),
        )
        .await
        .unwrap();
    });

    let mut conn = Connection::connect(&socket.path).await.unwrap();
    let candidate_id = promote_propose_over_connection(
        &mut conn,
        "router".to_string(),
        "tool-selection".to_string(),
        12,
        false,
    )
    .await
    .expect("propose");
    assert_eq!(candidate_id, "cand-test-1");

    server.await.unwrap();
}

#[tokio::test]
async fn propose_surfaces_a_rejection_verbatim() {
    let (socket, listener) = MockSocket::bind();
    let server = tokio::spawn(async move {
        let (mut stream, _addr) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        handshake_and_bind(&mut stream).await;
        let propose = read_envelope(&mut stream).await.unwrap().unwrap();
        write_envelope(
            &mut stream,
            &Envelope::reply_to(
                &propose,
                Payload::CommandRejected(CodypendentError::new(
                    "promotion.invalid-kind",
                    "unrecognized artifact kind",
                    false,
                )),
            ),
        )
        .await
        .unwrap();
    });

    let mut conn = Connection::connect(&socket.path).await.unwrap();
    let error = promote_propose_over_connection(
        &mut conn,
        "not-a-kind".to_string(),
        "x".to_string(),
        1,
        false,
    )
    .await
    .expect_err("an invalid kind must be surfaced, not silently accepted");
    assert!(error.to_string().contains("promotion.invalid-kind"));

    server.await.unwrap();
}

/// Drive one `promotion_command_over_connection` call against a mock that
/// asserts the exact `CommandBody` it receives, then replies `accepted`.
async fn drive_accepted(body: CommandBody, expect: impl Fn(&CommandBody) + Send + 'static) {
    let (socket, listener) = MockSocket::bind();
    let server = tokio::spawn(async move {
        let (mut stream, _addr) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        handshake_and_bind(&mut stream).await;
        let request = read_envelope(&mut stream).await.unwrap().unwrap();
        expect(&expect_command(&request).body);
        write_envelope(
            &mut stream,
            &Envelope::reply_to(
                &request,
                Payload::CommandAccepted {
                    command_id: command_id_of(&request),
                    sequence: None,
                    created_run: None,
                },
            ),
        )
        .await
        .unwrap();
    });

    let mut conn = Connection::connect(&socket.path).await.unwrap();
    promotion_command_over_connection(&mut conn, body, "test-verb")
        .await
        .expect("accepted");
    server.await.unwrap();
}

#[tokio::test]
async fn advance_sends_the_chosen_action() {
    drive_accepted(
        CommandBody::AdvancePromotion {
            candidate_id: "cand-1".to_string(),
            action: PromotionAction::ObserveCanary { regressed: true },
        },
        |body| match body {
            CommandBody::AdvancePromotion {
                candidate_id,
                action,
            } => {
                assert_eq!(candidate_id, "cand-1");
                assert_eq!(action, &PromotionAction::ObserveCanary { regressed: true });
            }
            other => panic!("expected AdvancePromotion, got {other:?}"),
        },
    )
    .await;
}

#[tokio::test]
async fn approve_sends_only_the_candidate_id_no_actor_field_on_the_wire() {
    // The security-relevant property at the wire level: there is no field a
    // client could use to *claim* an actor. Confirmed here by construction —
    // `CommandBody::ApprovePromotion` has exactly one field.
    drive_accepted(
        CommandBody::ApprovePromotion {
            candidate_id: "cand-2".to_string(),
        },
        |body| match body {
            CommandBody::ApprovePromotion { candidate_id } => {
                assert_eq!(candidate_id, "cand-2");
            }
            other => panic!("expected ApprovePromotion, got {other:?}"),
        },
    )
    .await;
}

#[tokio::test]
async fn rollback_rejection_is_surfaced_verbatim() {
    let (socket, listener) = MockSocket::bind();
    let server = tokio::spawn(async move {
        let (mut stream, _addr) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        handshake_and_bind(&mut stream).await;
        let request = read_envelope(&mut stream).await.unwrap().unwrap();
        assert!(matches!(
            expect_command(&request).body,
            CommandBody::RollbackPromotion { .. }
        ));
        write_envelope(
            &mut stream,
            &Envelope::reply_to(
                &request,
                Payload::CommandRejected(CodypendentError::new(
                    "promotion.illegal-transition",
                    "cannot rollback a candidate in stage Draft",
                    false,
                )),
            ),
        )
        .await
        .unwrap();
    });

    let mut conn = Connection::connect(&socket.path).await.unwrap();
    let error = promotion_command_over_connection(
        &mut conn,
        CommandBody::RollbackPromotion {
            candidate_id: "cand-3".to_string(),
        },
        "rollback",
    )
    .await
    .expect_err("rejection must propagate");
    assert!(error.to_string().contains("promotion.illegal-transition"));

    server.await.unwrap();
}
