//! STEP 1.13 headless JSONL client tests.
//!
//! No daemon dependency: a hand-rolled mock server, built only from
//! `codypendent_protocol`'s `read_envelope`/`write_envelope` framing, plays
//! the daemon's side of the STEP 1.11 handshake/command protocol over a real
//! `tokio::net::UnixListener`. This exercises `codypendent_cli::commands`'
//! connected core (`run_over_connection`) exactly as `codypendent run --jsonl`
//! does, without spawning `codypendentd` or `codypendent` as a subprocess.

use std::time::Duration;

use codypendent_cli::commands::run_over_connection;
use codypendent_cli::connection::Connection;
use codypendent_cli::stream::RunExit;
use codypendent_protocol::{
    read_envelope, write_envelope, Actor, AgentMode, Catchup, ClientCapabilities, ClientId,
    Command, CommandBody, CommandId, DaemonInstanceId, Envelope, EventBody, Payload,
    RunDisposition, RunId, RunState, ServerHello, SessionEvent, SessionId, PROTOCOL_V1,
};
use tokio::net::{UnixListener, UnixStream};

/// A scripted `SessionEvent` the mock server pushes after `StartRun`.
fn event(sequence: u64, body: EventBody) -> SessionEvent {
    SessionEvent {
        sequence,
        occurred_at: chrono::Utc::now(),
        causation_id: None,
        correlation_id: None,
        actor: Actor::System,
        body,
    }
}

/// A short-lived Unix socket under a fresh temp dir, isolated per test.
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

/// The command body's `command_id`, needed to correlate a `CommandAccepted`
/// reply — pulled back out of the envelope the client just sent.
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

/// Play the daemon's side of one `run --jsonl` session end to end:
/// `ClientHello` -> `ServerHello`; `CreateSession` -> `CommandAccepted` with
/// the new session id on the *envelope* (see `commands::run_over_connection`'s
/// doc comment for why); `AttachSession` -> an empty `Catchup::Events`;
/// `StartRun` -> `CommandAccepted`; then the scripted events, one per frame.
async fn mock_daemon(mut stream: UnixStream, session_id: SessionId, events: Vec<SessionEvent>) {
    // 1. Handshake.
    let hello = read_envelope(&mut stream)
        .await
        .expect("read ClientHello")
        .expect("connection open");
    assert!(matches!(hello.payload, Payload::ClientHello(_)));
    let server_hello = ServerHello {
        resume_token: None,
        selected_protocol: PROTOCOL_V1,
        daemon_version: "mock".to_string(),
        daemon_instance: DaemonInstanceId::new(),
        heartbeat_interval_ms: 15_000,
    };
    write_envelope(
        &mut stream,
        &Envelope::reply_to(&hello, Payload::ServerHello(server_hello)),
    )
    .await
    .expect("write ServerHello");

    // 2. CreateSession -> CommandAccepted, session id on the envelope.
    let create = read_envelope(&mut stream)
        .await
        .expect("read CreateSession")
        .expect("connection open");
    assert!(matches!(
        expect_command(&create).body,
        CommandBody::CreateSession { .. }
    ));
    let mut accepted = Envelope::reply_to(
        &create,
        Payload::CommandAccepted {
            command_id: command_id_of(&create),
            sequence: Some(1),
            created_run: None,
        },
    );
    accepted.session_id = Some(session_id);
    write_envelope(&mut stream, &accepted)
        .await
        .expect("write CreateSession CommandAccepted");

    // 3. AttachSession -> empty Catchup (nothing persisted yet in this mock).
    let attach = read_envelope(&mut stream)
        .await
        .expect("read AttachSession")
        .expect("connection open");
    match &expect_command(&attach).body {
        CommandBody::AttachSession {
            session_id: attached,
            ..
        } => assert_eq!(*attached, session_id),
        other => panic!("expected AttachSession, got {other:?}"),
    }
    write_envelope(
        &mut stream,
        &Envelope::reply_to(
            &attach,
            Payload::Catchup {
                catchup: Catchup::Events {
                    from: 1,
                    through: 0,
                    events: vec![],
                },
            },
        ),
    )
    .await
    .expect("write Catchup");

    // 4. StartRun -> CommandAccepted.
    let start = read_envelope(&mut stream)
        .await
        .expect("read StartRun")
        .expect("connection open");
    assert!(matches!(
        expect_command(&start).body,
        CommandBody::StartRun { .. }
    ));
    write_envelope(
        &mut stream,
        &Envelope::reply_to(
            &start,
            Payload::CommandAccepted {
                command_id: command_id_of(&start),
                sequence: Some(2),
                created_run: None,
            },
        ),
    )
    .await
    .expect("write StartRun CommandAccepted");

    // 5. The scripted event sequence, each as a server-forwarded Event.
    for scripted in events {
        let mut envelope = Envelope::request(ClientId::new(), Payload::Event(scripted));
        envelope.session_id = Some(session_id);
        write_envelope(&mut stream, &envelope)
            .await
            .expect("write scripted event");
    }
}

/// Drive one `run_over_connection` call against a mock server scripted with
/// `events`, returning the client's captured JSONL bytes and its `RunExit`.
async fn drive(events: Vec<SessionEvent>) -> (Vec<u8>, RunExit) {
    let (socket, listener) = MockSocket::bind();
    let session_id = SessionId::new();

    let server_events = events.clone();
    let server = tokio::spawn(async move {
        let (stream, _addr) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("mock server accepted a connection in time")
            .expect("accept");
        mock_daemon(stream, session_id, server_events).await;
    });

    let mut conn = Connection::connect(&socket.path)
        .await
        .expect("client connects to mock socket");
    let mut out = Vec::new();
    let exit = tokio::time::timeout(
        Duration::from_secs(5),
        run_over_connection(
            &mut conn,
            "diagnose the failing test".to_string(),
            AgentMode::Build,
            "/repo/under/test",
            &mut out,
        ),
    )
    .await
    .expect("run_over_connection completed in time")
    .expect("run_over_connection succeeded");

    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("mock server task finished in time")
        .expect("mock server task did not panic");

    (out, exit)
}

/// Parse `out` as JSONL and return the decoded envelopes, asserting every
/// line is independently parseable as an `Envelope`.
fn parse_jsonl(out: &[u8]) -> Vec<Envelope> {
    let text = std::str::from_utf8(out).expect("stdout is valid UTF-8");
    text.lines()
        .map(|line| {
            serde_json::from_str::<Envelope>(line)
                .unwrap_or_else(|e| panic!("line does not parse as an Envelope: {e}\n{line}"))
        })
        .collect()
}

fn run_started(run_id: RunId) -> SessionEvent {
    event(
        3,
        EventBody::RunStarted {
            run_id,
            objective: "diagnose the failing test".to_string(),
            mode: AgentMode::Build,
        },
    )
}

fn model_delta(run_id: RunId) -> SessionEvent {
    event(
        4,
        EventBody::ModelStreamDelta {
            run_id,
            text: "inspecting the failing test...".to_string(),
        },
    )
}

fn artifact_ref() -> codypendent_protocol::ArtifactRef {
    codypendent_protocol::ArtifactRef {
        id: codypendent_protocol::ArtifactId::new(),
        media_type: "application/json".to_string(),
        byte_length: 42,
        sha256: "0".repeat(64),
        sensitivity: codypendent_protocol::DataClassification::Internal,
    }
}

#[tokio::test]
async fn jsonl_lines_parse_and_match_the_scripted_sequence_in_order() {
    let run_id = RunId::new();
    let scripted = vec![
        run_started(run_id),
        model_delta(run_id),
        event(
            5,
            EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Completed {
                    summary: Some("fixed the parser".to_string()),
                },
                chronicle: artifact_ref(),
            },
        ),
    ];

    let (out, exit) = drive(scripted.clone()).await;

    let envelopes = parse_jsonl(&out);
    assert_eq!(
        envelopes.len(),
        scripted.len(),
        "every scripted event produced exactly one JSONL line"
    );
    for (envelope, expected) in envelopes.iter().zip(scripted.iter()) {
        match &envelope.payload {
            Payload::Event(actual) => assert_eq!(actual, expected, "events stream in order"),
            other => panic!("expected an Event envelope, got {other:?}"),
        }
    }

    assert_eq!(exit, RunExit::Completed);
    assert_eq!(exit.exit_code(), 0);
}

#[tokio::test]
async fn failed_run_maps_to_exit_code_two() {
    let run_id = RunId::new();
    let scripted = vec![
        run_started(run_id),
        event(
            4,
            EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Failed {
                    reason: "cargo test still fails".to_string(),
                },
                chronicle: artifact_ref(),
            },
        ),
    ];

    let (out, exit) = drive(scripted.clone()).await;

    let envelopes = parse_jsonl(&out);
    assert_eq!(envelopes.len(), scripted.len());
    assert_eq!(exit, RunExit::Failed);
    assert_eq!(exit.exit_code(), 2);
}

#[tokio::test]
async fn cancelled_run_maps_to_exit_code_130() {
    let run_id = RunId::new();
    // A cancellation observed via `RunStateChanged` rather than
    // `RunCompleted` — both are documented terminal signals (STEP 1.13).
    let scripted = vec![
        run_started(run_id),
        event(
            4,
            EventBody::RunStateChanged {
                run_id,
                state: RunState::Cancelled,
            },
        ),
    ];

    let (out, exit) = drive(scripted.clone()).await;

    let envelopes = parse_jsonl(&out);
    assert_eq!(envelopes.len(), scripted.len());
    assert_eq!(exit, RunExit::Cancelled);
    assert_eq!(exit.exit_code(), 130);
}

#[tokio::test]
async fn a_different_runs_event_is_forwarded_but_not_treated_as_terminal() {
    let our_run = RunId::new();
    let other_run = RunId::new();
    let scripted = vec![
        run_started(our_run),
        // A concurrently started, unrelated run finishes first — it must not
        // end *our* stream.
        event(
            4,
            EventBody::RunCompleted {
                run_id: other_run,
                disposition: RunDisposition::Completed { summary: None },
                chronicle: artifact_ref(),
            },
        ),
        event(
            5,
            EventBody::RunCompleted {
                run_id: our_run,
                disposition: RunDisposition::Completed {
                    summary: Some("actually ours".to_string()),
                },
                chronicle: artifact_ref(),
            },
        ),
    ];

    let (out, exit) = drive(scripted.clone()).await;

    let envelopes = parse_jsonl(&out);
    // Every scripted event is still forwarded to the client (Chapter 03: a
    // client observes everything it is subscribed to)...
    assert_eq!(envelopes.len(), scripted.len());
    // ...but the stream only ends on *our* run's terminal event.
    assert_eq!(exit, RunExit::Completed);
}

// Sanity-check `ClientCapabilities::default()` stays wired through the
// handshake this test suite relies on (a change here would silently break
// every test above via a bad `ClientHello`).
#[test]
fn client_capabilities_default_is_all_false() {
    let caps = ClientCapabilities::default();
    assert!(!caps.rich_text);
    assert!(!caps.mouse);
}
