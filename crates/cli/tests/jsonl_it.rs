//! STEP 1.13 headless JSONL client tests.
//!
//! No daemon dependency: a hand-rolled mock server, built only from
//! `codypendent_protocol`'s `read_envelope`/`write_envelope` framing, plays
//! the daemon's side of the STEP 1.11 handshake/command protocol over a real
//! `tokio::net::UnixListener`. This exercises `codypendent_cli::commands`'
//! connected core (`run_over_connection`) exactly as `codypendent run --jsonl`
//! does, without spawning `codypendentd` or `codypendent` as a subprocess.

use std::time::Duration;

use codypendent_cli::commands::run_over_connection_with_format;
use codypendent_cli::connection::Connection;
use codypendent_cli::stream::{OutputFormat, RunExit};
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
        build_id: String::new(),
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
    drive_format(events, OutputFormat::Jsonl).await
}

async fn drive_format(events: Vec<SessionEvent>, format: OutputFormat) -> (Vec<u8>, RunExit) {
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
        run_over_connection_with_format(
            &mut conn,
            "diagnose the failing test".to_string(),
            AgentMode::Build,
            "/repo/under/test",
            None,
            &mut out,
            format,
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
            thought: false,
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
                    error: None,
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

#[tokio::test]
async fn a_failed_runs_reason_reaches_the_client_not_only_daemon_log() {
    // The daemon's real terminal path (`crates/daemon/src/recovery.rs::fail_run`,
    // and the plain agent loop's own finish in `crates/runtime/src/agent.rs`)
    // always publishes a bare `RunStateChanged{Failed}` immediately followed,
    // in the same transaction, by a `RunCompleted{disposition: Failed{reason}}`
    // carrying the human-readable reason. This is the exact two-event sequence
    // a real "no model configured" failure produces — scripted here rather
    // than only a lone `RunCompleted` (as `failed_run_maps_to_exit_code_two`
    // does) so this test can catch a regression to "stop at the first
    // terminal-shaped event", which would silently drop the second event and
    // its `reason` — the review's exact finding (F7): three real failed runs
    // that surfaced nothing but `RunStateChanged{Failed}` on `--jsonl`, the
    // actual reason visible only in `daemon.log`.
    let run_id = RunId::new();
    let scripted = vec![
        run_started(run_id),
        event(
            4,
            EventBody::RunStateChanged {
                run_id,
                state: RunState::Failed,
            },
        ),
        event(
            5,
            EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Failed {
                    reason: "no model configured: every candidate failed for Build".to_string(),
                    error: None,
                },
                chronicle: artifact_ref(),
            },
        ),
    ];

    let (out, exit) = drive(scripted.clone()).await;

    let envelopes = parse_jsonl(&out);
    // Both terminal-shaped events reached the client's own JSONL output — not
    // just the first of the two.
    assert_eq!(
        envelopes.len(),
        scripted.len(),
        "the stream must not end at RunStateChanged, before RunCompleted's \
         reason ever arrives"
    );
    let reason_line = std::str::from_utf8(&out)
        .expect("utf8")
        .lines()
        .find(|line| line.contains("no model configured"));
    assert!(
        reason_line.is_some(),
        "the client's own JSONL output must carry the failure reason \
         (RunCompleted's disposition), not leave it only in daemon.log:\n{}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(exit, RunExit::Failed);
    assert_eq!(exit.exit_code(), 2);
}

/// Outcome 20 (L3): the run's MEASURED usage reaches `--jsonl` stdout.
///
/// `RunUsage` is published AFTER the run's terminal state — the executor
/// journals `RunCompleted`, then records usage and emits it — so returning on
/// `RunCompleted` closed the connection in the gap. The reviewer measured
/// `13 RunCompleted / 14 RunUsage` in the ledger against `[1 … 13]` on stdout,
/// across six runs, with `RunUsage` printed zero times. Revert
/// `drain_trailing_usage` and this test fails on the line count.
#[tokio::test]
async fn measured_usage_published_after_the_terminal_event_still_reaches_stdout() {
    let run_id = RunId::new();
    let scripted = vec![
        run_started(run_id),
        event(
            4,
            EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Completed {
                    summary: Some("fixed the parser".to_string()),
                },
                chronicle: artifact_ref(),
            },
        ),
        // Exactly the daemon's ordering: usage trails the terminal event.
        event(
            5,
            EventBody::RunUsage {
                run_id,
                prompt_tokens: Some(9_000),
                completion_tokens: Some(1_000),
                cost_micros: Some(42_000),
                coverage: None,
            },
        ),
    ];

    let (out, exit) = drive(scripted.clone()).await;

    let envelopes = parse_jsonl(&out);
    assert_eq!(
        envelopes.len(),
        scripted.len(),
        "the stream must not end at RunCompleted, before RunUsage arrives:\n{}",
        String::from_utf8_lossy(&out)
    );
    let usage_line = std::str::from_utf8(&out)
        .expect("utf8")
        .lines()
        .find(|line| line.contains("RunUsage"))
        .unwrap_or_else(|| {
            panic!(
                "no RunUsage line in the client's own output:\n{}",
                String::from_utf8_lossy(&out)
            )
        });
    assert!(
        usage_line.contains("9000") && usage_line.contains("1000"),
        "the measured token counts must survive to stdout: {usage_line}"
    );
    // Draining usage must not change the run's verdict.
    assert_eq!(exit, RunExit::Completed);
    assert_eq!(exit.exit_code(), 0);
}

/// The drain is bounded: a daemon that measured nothing publishes no
/// `RunUsage` at all (an all-`None` event would be indistinguishable from a
/// genuinely free run), and an older daemon never publishes one. Neither may
/// hang `run --jsonl` or change its exit code — the `drive` helper's own 5s
/// timeout is the assertion that it returns.
#[tokio::test]
async fn a_run_that_publishes_no_usage_still_exits_promptly() {
    let run_id = RunId::new();
    let scripted = vec![
        run_started(run_id),
        event(
            4,
            EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Completed { summary: None },
                chronicle: artifact_ref(),
            },
        ),
    ];

    let (out, exit) = drive(scripted.clone()).await;

    assert_eq!(parse_jsonl(&out).len(), scripted.len());
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

#[tokio::test]
async fn text_output_streams_only_the_requested_runs_answer_and_keeps_its_exit_status() {
    let run = RunId::new();
    let sibling = RunId::new();
    let (out, exit) = drive_format(
        vec![
            run_started(run),
            event(
                3,
                EventBody::ModelStreamDelta {
                    run_id: run,
                    text: "private reasoning".into(),
                    thought: true,
                },
            ),
            event(
                4,
                EventBody::ModelStreamDelta {
                    run_id: run,
                    text: "Hello ".into(),
                    thought: false,
                },
            ),
            event(
                5,
                EventBody::ModelStreamDelta {
                    run_id: sibling,
                    text: "sibling output".into(),
                    thought: false,
                },
            ),
            event(
                6,
                EventBody::RunCompleted {
                    run_id: sibling,
                    disposition: RunDisposition::Failed {
                        reason: "sibling failed".into(),
                        error: None,
                    },
                    chronicle: artifact_ref(),
                },
            ),
            event(
                7,
                EventBody::ModelStreamDelta {
                    run_id: run,
                    text: "世界\x1b[2J".into(),
                    thought: false,
                },
            ),
            event(
                8,
                EventBody::RunCompleted {
                    run_id: run,
                    disposition: RunDisposition::Completed {
                        summary: Some("must not duplicate the answer".into()),
                    },
                    chronicle: artifact_ref(),
                },
            ),
        ],
        OutputFormat::Text,
    )
    .await;
    assert_eq!(String::from_utf8(out).unwrap(), "Hello 世界\n");
    assert_eq!(exit, RunExit::Completed);
}

#[tokio::test]
async fn text_output_uses_a_completion_summary_when_no_answer_was_streamed() {
    let run = RunId::new();
    let (out, exit) = drive_format(
        vec![
            run_started(run),
            event(
                3,
                EventBody::RunCompleted {
                    run_id: run,
                    disposition: RunDisposition::Completed {
                        summary: Some("Done".into()),
                    },
                    chronicle: artifact_ref(),
                },
            ),
        ],
        OutputFormat::Text,
    )
    .await;
    assert_eq!(out, b"Done\n");
    assert_eq!(exit, RunExit::Completed);
}

#[tokio::test]
async fn failed_and_cancelled_text_runs_keep_the_jsonl_exit_contract() {
    for (disposition, expected) in [
        (
            RunDisposition::Failed {
                reason: "provider unavailable".into(),
                error: None,
            },
            RunExit::Failed,
        ),
        (
            RunDisposition::Cancelled { reason: None },
            RunExit::Cancelled,
        ),
    ] {
        let run = RunId::new();
        let (out, exit) = drive_format(
            vec![
                run_started(run),
                event(
                    3,
                    EventBody::RunCompleted {
                        run_id: run,
                        disposition,
                        chronicle: artifact_ref(),
                    },
                ),
            ],
            OutputFormat::Text,
        )
        .await;
        assert!(out.is_empty());
        assert_eq!(exit, expected);
    }
}
