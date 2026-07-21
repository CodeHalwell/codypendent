//! Session-server behaviour over a real Unix socket (STEP 1.11): handshake,
//! attach + catch-up (events and snapshot), multi-client fan-out, resume from a
//! sequence, and role enforcement. Drives `server::run` exactly like
//! `tests/socket.rs`, exchanging framed envelopes over a `UnixStream`.

use std::time::Duration;

use codypendent_daemon::{db, instance, ledger, server};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    read_envelope, write_envelope, Actor, AgentMode, Catchup, ClientCapabilities, ClientHello,
    ClientId, ClientRole, Command, CommandBody, CommandId, Envelope, EventBody, Payload,
    SessionEvent, SessionId, Subscription, WorkspaceId, PROTOCOL_V1,
};
use sqlx::SqlitePool;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

type ServerTask = JoinHandle<anyhow::Result<()>>;

/// Boot a daemon on a temp data dir; return its paths and the server task.
async fn start_server(tmp: &tempfile::TempDir) -> (RuntimePaths, ServerTask) {
    let paths = RuntimePaths::from_data_dir(tmp.path().to_path_buf());
    paths.ensure_directories().expect("create directories");
    let pool = db::open_database(&paths.data_dir.join("codypendent.db"))
        .await
        .expect("open db");
    let boot = instance::record_boot(&pool).await.expect("record boot");
    let task = tokio::spawn(server::run(pool, paths.clone(), boot));
    (paths, task)
}

/// A second pool onto the same database file, for tests that seed the ledger
/// directly (WAL mode allows concurrent readers/writers).
async fn client_pool(paths: &RuntimePaths) -> SqlitePool {
    db::open_database(&paths.data_dir.join("codypendent.db"))
        .await
        .expect("open db (client)")
}

async fn connect(paths: &RuntimePaths) -> UnixStream {
    loop {
        match UnixStream::connect(&paths.socket_path).await {
            Ok(stream) => break stream,
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
}

/// Read the next frame with a generous timeout so a hang fails fast.
async fn read_frame(stream: &mut UnixStream) -> Envelope {
    tokio::time::timeout(Duration::from_secs(5), read_envelope(stream))
        .await
        .expect("read timed out")
        .expect("read frame")
        .expect("server must reply")
}

async fn send_recv(stream: &mut UnixStream, request: &Envelope) -> Envelope {
    write_envelope(stream, request).await.expect("write frame");
    read_frame(stream).await
}

fn command(body: CommandBody, key: &str) -> Command {
    Command {
        command_id: CommandId::new(),
        idempotency_key: key.to_string(),
        expected_revision: None,
        body,
    }
}

/// Handshake and assert the negotiated protocol.
async fn handshake(stream: &mut UnixStream, client_id: ClientId) {
    let hello = ClientHello {
        client_name: "server-it".to_string(),
        client_version: "0.0.0".to_string(),
        supported_protocols: vec![PROTOCOL_V1],
        capabilities: ClientCapabilities::default(),
        resume_token: None,
    };
    let reply = send_recv(
        stream,
        &Envelope::request(client_id, Payload::ClientHello(hello)),
    )
    .await;
    match reply.payload {
        Payload::ServerHello(server_hello) => {
            assert_eq!(server_hello.selected_protocol, PROTOCOL_V1);
            assert_eq!(server_hello.heartbeat_interval_ms, 15_000);
        }
        other => panic!("expected ServerHello, got {other:?}"),
    }
}

/// Attach to a session and return the catch-up. `key` is arbitrary — attach is
/// intercepted before the idempotent write path.
async fn attach(
    stream: &mut UnixStream,
    client_id: ClientId,
    session_id: SessionId,
    last_seen: Option<u64>,
    role: ClientRole,
    subscriptions: Vec<Subscription>,
    key: &str,
) -> Catchup {
    let body = CommandBody::AttachSession {
        session_id,
        last_seen_sequence: last_seen,
        subscriptions,
        requested_role: role,
    };
    let reply = send_recv(
        stream,
        &Envelope::request(client_id, Payload::Command(command(body, key))),
    )
    .await;
    match reply.payload {
        Payload::Catchup { catchup } => catchup,
        other => panic!("expected Catchup, got {other:?}"),
    }
}

/// Bind `role` to the connection via an attach targeting a throwaway session
/// id. The attach itself is REJECTED (`protocol.session-not-found`) — the
/// daemon refuses to fabricate an empty catch-up for an unknown session — but
/// the requested role still binds to the connection, which is all a role
/// bootstrap needs.
async fn bind_role(stream: &mut UnixStream, client_id: ClientId, role: ClientRole, key: &str) {
    let reply = send_recv(
        stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::AttachSession {
                    session_id: SessionId::new(),
                    last_seen_sequence: None,
                    subscriptions: vec![Subscription::SessionSummary],
                    requested_role: role,
                },
                key,
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::Error(error) => assert_eq!(error.code, "protocol.session-not-found"),
        other => panic!("expected session-not-found for role bootstrap, got {other:?}"),
    }
}

/// Read frames (ignoring stray heartbeat Pings) until an `Event` arrives.
async fn read_until_event(stream: &mut UnixStream) -> SessionEvent {
    for _ in 0..6 {
        match read_frame(stream).await.payload {
            Payload::Event(event) => return event,
            Payload::Ping => continue,
            other => panic!("expected Event, got {other:?}"),
        }
    }
    panic!("no Event arrived");
}

async fn shutdown(mut stream: UnixStream, task: ServerTask) {
    write_envelope(
        &mut stream,
        &Envelope::request(ClientId::new(), Payload::Shutdown),
    )
    .await
    .expect("write shutdown");
    // Skip any frames still in flight when Shutdown is sent — a heartbeat Ping or
    // an event/presence notification the subscription queued after catchup — until
    // the ShutdownAck arrives. (The undrained frame is delivery-order dependent,
    // which made a single read here flaky under CI scheduling.)
    let mut acked = false;
    for _ in 0..16 {
        if matches!(read_frame(&mut stream).await.payload, Payload::ShutdownAck) {
            acked = true;
            break;
        }
    }
    assert!(acked, "ShutdownAck must arrive");
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("server stops within 5s")
        .expect("server task must not panic")
        .expect("server stops cleanly");
}

fn note_event(sequence: u64, text: &str) -> SessionEvent {
    SessionEvent {
        sequence,
        occurred_at: chrono::Utc::now(),
        causation_id: None,
        correlation_id: None,
        actor: Actor::System,
        body: EventBody::NoteAppended {
            text: text.to_string(),
            run_id: None,
        },
    }
}

/// The single session id currently stored (used to learn a created session's id,
/// which `CommandAccepted` does not carry).
async fn only_session_id(pool: &SqlitePool) -> SessionId {
    let (id,): (String,) = sqlx::query_as("SELECT id FROM sessions LIMIT 1")
        .fetch_one(pool)
        .await
        .expect("one session");
    id.parse().expect("valid session id")
}

#[tokio::test]
async fn handshake_returns_server_hello() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let mut stream = connect(&paths).await;
    handshake(&mut stream, ClientId::new()).await;
    shutdown(stream, task).await;
}

#[tokio::test]
async fn create_attach_and_two_clients_observe_one_event() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;

    // --- Client 1: handshake, establish a Contributor role, create a session ---
    let c1 = ClientId::new();
    let mut s1 = connect(&paths).await;
    handshake(&mut s1, c1).await;
    // CreateSession needs a role; establish Contributor via a role-bootstrap
    // attach (rejected for the unknown session, but the role binds).
    bind_role(&mut s1, c1, ClientRole::Contributor, "att-role").await;

    let reply = send_recv(
        &mut s1,
        &Envelope::request(
            c1,
            Payload::Command(command(
                CommandBody::CreateSession {
                    workspace: WorkspaceId::new(),
                    title: "diagnose the failing test".to_string(),
                },
                "create-1",
            )),
        ),
    )
    .await;
    assert!(matches!(reply.payload, Payload::CommandAccepted { .. }));
    let session_id = only_session_id(&pool).await;

    // Client 1 attaches to the real session -> Catchup::Events with SessionCreated.
    let catchup = attach(
        &mut s1,
        c1,
        session_id,
        None,
        ClientRole::Contributor,
        vec![Subscription::SessionSummary],
        "att-1",
    )
    .await;
    match catchup {
        Catchup::Events {
            from,
            through,
            events,
        } => {
            assert_eq!(from, 1);
            assert_eq!(through, 1);
            assert_eq!(events.len(), 1);
            assert!(matches!(events[0].body, EventBody::SessionCreated { .. }));
        }
        other => panic!("expected Events catchup, got {other:?}"),
    }

    // --- Client 2: handshake, attach to the same session ---
    let c2 = ClientId::new();
    let mut s2 = connect(&paths).await;
    handshake(&mut s2, c2).await;
    let _ = attach(
        &mut s2,
        c2,
        session_id,
        None,
        ClientRole::Contributor,
        vec![Subscription::SessionSummary],
        "att-2",
    )
    .await;

    // Client 2 submits a command that appends an event; it observes both the
    // acknowledgement and the resulting event (any order over one socket).
    write_envelope(
        &mut s2,
        &Envelope::request(
            c2,
            Payload::Command(command(
                CommandBody::SubmitUserInput {
                    session_id,
                    text: "focus on the parser".to_string(),
                    mode: AgentMode::Build,
                },
                "input-1",
            )),
        ),
    )
    .await
    .expect("write submit");

    let mut got_accepted = false;
    let mut got_event = false;
    for _ in 0..8 {
        match read_frame(&mut s2).await.payload {
            Payload::CommandAccepted { .. } => got_accepted = true,
            Payload::Event(event) => match event.body {
                EventBody::NoteAppended { .. } => got_event = true,
                // Presence is expected background noise (STEP 3.7): a client's own
                // attach publishes a `ClientPresenceChanged` it then observes.
                EventBody::ClientPresenceChanged { .. } => {}
                other => panic!("unexpected event on client 2: {other:?}"),
            },
            Payload::Ping => {}
            other => panic!("unexpected frame on client 2: {other:?}"),
        }
        if got_accepted && got_event {
            break;
        }
    }
    assert!(got_accepted, "submitter must receive CommandAccepted");
    assert!(got_event, "submitter (subscribed) must observe the event");

    // Client 1, the second observer of the same session, receives the same event
    // (skipping the presence events either client's attach produced).
    let observed = loop {
        let event = read_until_event(&mut s1).await;
        if !matches!(event.body, EventBody::ClientPresenceChanged { .. }) {
            break event;
        }
    };
    assert!(matches!(observed.body, EventBody::NoteAppended { .. }));

    shutdown(s1, task).await;
    drop(s2);
}

#[tokio::test]
async fn resume_from_sequence_returns_exactly_missed_events() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;

    // Seed a session with five events directly on the ledger.
    let session_id = SessionId::new();
    ledger::create_session(&pool, session_id, "resume")
        .await
        .unwrap();
    for sequence in 1..=5u64 {
        ledger::append_event(
            &pool,
            session_id,
            &note_event(sequence, &format!("n{sequence}")),
        )
        .await
        .unwrap();
    }

    let client_id = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, client_id).await;

    // Reconnect from sequence 2 -> exactly events 3, 4, 5 in order.
    let catchup = attach(
        &mut stream,
        client_id,
        session_id,
        Some(2),
        ClientRole::Observer,
        vec![Subscription::SessionSummary],
        "resume-att",
    )
    .await;
    match catchup {
        Catchup::Events {
            from,
            through,
            events,
        } => {
            assert_eq!(from, 3);
            assert_eq!(through, 5);
            let sequences: Vec<u64> = events.iter().map(|event| event.sequence).collect();
            assert_eq!(sequences, vec![3, 4, 5]);
        }
        other => panic!("expected Events catchup, got {other:?}"),
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn attach_far_behind_returns_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;

    // 501 events with last_seen 0 -> gap 501 > 500 -> snapshot path.
    let session_id = SessionId::new();
    ledger::create_session(&pool, session_id, "big session")
        .await
        .unwrap();
    for sequence in 1..=501u64 {
        ledger::append_event(&pool, session_id, &note_event(sequence, "x"))
            .await
            .unwrap();
    }

    let client_id = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, client_id).await;

    let catchup = attach(
        &mut stream,
        client_id,
        session_id,
        Some(0),
        ClientRole::Observer,
        vec![Subscription::SessionSummary],
        "snap-att",
    )
    .await;
    match catchup {
        Catchup::Snapshot {
            through,
            projection,
        } => {
            assert_eq!(through, 501);
            assert_eq!(projection.last_sequence, 501);
            assert_eq!(projection.session_id, session_id);
        }
        other => panic!("expected Snapshot catchup, got {other:?}"),
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn observer_start_run_is_role_denied() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;

    let client_id = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, client_id).await;

    // Bind Observer, then attempt StartRun. Role is checked before session
    // existence, so an Observer is denied regardless of the target session.
    let session_id = SessionId::new();
    bind_role(&mut stream, client_id, ClientRole::Observer, "obs-att").await;

    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::StartRun {
                    session_id,
                    objective: "diagnose".to_string(),
                    mode: AgentMode::Build,
                    repository: None,
                },
                "start-denied",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.role-denied"),
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn observer_cannot_acquire_a_document_lease() {
    // A lease is a precursor to writing, so — like StartRun — an Observer is denied
    // before the daemon even checks whether document transport is wired.
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;

    let client_id = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, client_id).await;
    bind_role(
        &mut stream,
        client_id,
        ClientRole::Observer,
        "obs-lease-att",
    )
    .await;

    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::AcquireDocumentLease {
                    lease: codypendent_protocol::DocumentEditLease {
                        document_id: codypendent_protocol::DocumentId::new(),
                        block_id: Some("p".to_string()),
                    },
                    ttl_seconds: None,
                },
                "lease-denied",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.role-denied"),
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn lease_commands_are_rejected_when_transport_is_unwired() {
    // The daemon's own test server injects no leaser (the executor-less path), so a
    // Contributor's lease command is rejected structurally rather than crashing the
    // connection — mirroring `MutateDocument` without a mutator.
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;

    let client_id = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, client_id).await;
    bind_role(
        &mut stream,
        client_id,
        ClientRole::Contributor,
        "contrib-lease-att",
    )
    .await;

    // Acquire → transport-unavailable.
    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::AcquireDocumentLease {
                    lease: codypendent_protocol::DocumentEditLease {
                        document_id: codypendent_protocol::DocumentId::new(),
                        block_id: None,
                    },
                    ttl_seconds: Some(60),
                },
                "lease-acquire-unwired",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => {
            assert_eq!(error.code, "document.transport-unavailable");
        }
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    // Release → same structural rejection (and the connection survives both).
    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ReleaseDocumentLease {
                    lease_id: "lease-x".to_string(),
                },
                "lease-release-unwired",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => {
            assert_eq!(error.code, "document.transport-unavailable");
        }
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn start_workflow_is_role_denied_then_transport_unavailable() {
    // Phase 5 STEP 5.2: `StartWorkflow` is intercepted at the connection level
    // like `MutateDocument`. The daemon's own test server injects no starter, so
    // the command is rejected structurally rather than crashing the connection —
    // and an Observer is role-denied before the starter is even consulted.
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;

    let manifest =
        "schema_version: 1\nid: wf\nversion: 1\nsteps:\n  - id: a\n    tool: repository.test\n";

    // An Observer is role-denied (the role is checked before the starter).
    let observer = ClientId::new();
    let mut obs_stream = connect(&paths).await;
    handshake(&mut obs_stream, observer).await;
    bind_role(
        &mut obs_stream,
        observer,
        ClientRole::Observer,
        "obs-wf-att",
    )
    .await;
    let reply = send_recv(
        &mut obs_stream,
        &Envelope::request(
            observer,
            Payload::Command(command(
                CommandBody::StartWorkflow {
                    manifest: manifest.to_string(),
                    inputs: serde_json::Value::Null,
                },
                "wf-observer",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.role-denied"),
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    // A Contributor gets past the role gate but the transport is unwired.
    let contributor = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, contributor).await;
    bind_role(
        &mut stream,
        contributor,
        ClientRole::Contributor,
        "contrib-wf-att",
    )
    .await;
    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            contributor,
            Payload::Command(command(
                CommandBody::StartWorkflow {
                    manifest: manifest.to_string(),
                    inputs: serde_json::Value::Null,
                },
                "wf-contributor",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => {
            assert_eq!(error.code, "workflow.transport-unavailable");
        }
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn workflow_lifecycle_requires_controller_then_transport_unavailable() {
    // Phase 5 STEP 5.2: pause/resume/retry are intercepted like `StartWorkflow`, but
    // controlling a run requires the `Controller` role (matching agent-run
    // cancel/pause/resume). A Contributor is role-denied; a Controller gets past the
    // gate but the daemon's own test server injects no lifecycle seam, so the command
    // is rejected `workflow.transport-unavailable` rather than crashing the connection.
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;

    // A Contributor may start a workflow but not control one → role-denied.
    let contributor = ClientId::new();
    let mut contrib_stream = connect(&paths).await;
    handshake(&mut contrib_stream, contributor).await;
    bind_role(
        &mut contrib_stream,
        contributor,
        ClientRole::Contributor,
        "contrib-life-att",
    )
    .await;
    let reply = send_recv(
        &mut contrib_stream,
        &Envelope::request(
            contributor,
            Payload::Command(command(
                CommandBody::PauseWorkflow {
                    workflow_run_id: "wfrun-1".to_string(),
                },
                "pause-contributor",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.role-denied"),
        other => panic!("expected role-denied, got {other:?}"),
    }

    // A Controller gets past the role gate; all three lifecycle commands then hit
    // the unwired transport.
    let controller = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, controller).await;
    bind_role(
        &mut stream,
        controller,
        ClientRole::Controller,
        "controller-life-att",
    )
    .await;
    for (body, key) in [
        (
            CommandBody::PauseWorkflow {
                workflow_run_id: "wfrun-1".to_string(),
            },
            "pause-controller",
        ),
        (
            CommandBody::ResumeWorkflow {
                workflow_run_id: "wfrun-1".to_string(),
            },
            "resume-controller",
        ),
        (
            CommandBody::RetryWorkflowNode {
                workflow_run_id: "wfrun-1".to_string(),
                node_id: "verify".to_string(),
            },
            "retry-controller",
        ),
    ] {
        let reply = send_recv(
            &mut stream,
            &Envelope::request(controller, Payload::Command(command(body, key))),
        )
        .await;
        match reply.payload {
            Payload::CommandRejected(error) => {
                assert_eq!(error.code, "workflow.transport-unavailable");
            }
            other => panic!("expected transport-unavailable for {key}, got {other:?}"),
        }
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn attaching_a_second_client_emits_presence() {
    // STEP 3.7: presence events let each attached client see who else is here.
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;

    let client_a = ClientId::new();
    let client_b = ClientId::new();

    // A handshakes, creates a session, and attaches (SessionSummary sees all).
    let mut a = connect(&paths).await;
    handshake(&mut a, client_a).await;
    let created = send_recv(
        &mut a,
        &Envelope::request(
            client_a,
            Payload::Command(command(
                CommandBody::CreateSession {
                    workspace: WorkspaceId::new(),
                    title: "handoff".to_string(),
                },
                "create-1",
            )),
        ),
    )
    .await;
    let session_id = created.session_id.expect("session id in CommandAccepted");
    attach(
        &mut a,
        client_a,
        session_id,
        Some(0),
        ClientRole::Controller,
        vec![Subscription::SessionSummary],
        "attach-a",
    )
    .await;

    // A receives its own arrival first.
    let own = read_until_event(&mut a).await;
    assert!(
        matches!(&own.body, EventBody::ClientPresenceChanged { client_id, present: true, .. } if *client_id == client_a),
        "expected A's own presence, got {:?}",
        own.body
    );

    // B handshakes and attaches as a Contributor (the handoff to an IDE).
    let mut b = connect(&paths).await;
    handshake(&mut b, client_b).await;
    attach(
        &mut b,
        client_b,
        session_id,
        Some(0),
        ClientRole::Contributor,
        vec![Subscription::SessionSummary],
        "attach-b",
    )
    .await;

    // A now sees B join, with B's role — the presence signal a handoff relies on.
    let joined = read_until_event(&mut a).await;
    assert!(
        matches!(
            &joined.body,
            EventBody::ClientPresenceChanged { client_id, role, present: true }
                if *client_id == client_b && *role == ClientRole::Contributor
        ),
        "expected B's presence, got {:?}",
        joined.body
    );

    drop(b);
    shutdown(a, task).await;
}
