//! The real-crash recovery test (STEP 1.14), living in the crate that builds the
//! `codypendentd` binary so `CARGO_BIN_EXE_codypendentd` is defined.
//!
//! It spawns the actual daemon binary against a temp data dir, creates a run over
//! the socket and parks it (`PauseRun` — a live state), `kill -9`s the child,
//! restarts it, and asserts the run recovered to `Failed` with a terminal
//! `RunCompleted` — exercising the assembly binary's `main.rs` startup wiring end
//! to end.
//!
//! With the run executor now wired in (this crate injects it), an accepted
//! `StartRun` also begins executing immediately; in a bare data dir with no
//! `models.toml` the run fails cleanly on its own. Either way the run reaches a
//! terminal `Failed` — via the executor, or via restart recovery of the parked
//! `Paused` projection — so the recovery contract asserted here still holds.

use std::path::Path;
use std::process::{Child, Command as StdCommand, Stdio};
use std::str::FromStr;
use std::time::Duration;

use codypendent_daemon::{db, ledger, projections};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    read_envelope, write_envelope, AgentMode, ClientCapabilities, ClientHello, ClientId,
    Command as ProtoCommand, CommandBody, CommandId, Envelope, EventBody, Payload, RunDisposition,
    RunId, RunState, SessionId, WorkspaceId, PROTOCOL_V1,
};
use sqlx::SqlitePool;
use tokio::net::UnixStream;

/// Spawn the `codypendentd` binary against a temp data dir, with a quiet log and
/// discarded output. The socket resolves under `<data_dir>/run/` (the data-dir
/// override branch of discovery), matching what the test connects to.
fn spawn_daemon(data_dir: &Path) -> Child {
    StdCommand::new(env!("CARGO_BIN_EXE_codypendentd"))
        .env("CODYPENDENT_DATA_DIR", data_dir)
        .env_remove("CODYPENDENT_SOCKET")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn codypendentd")
}

/// Poll-connect until the daemon's socket accepts (its recovery has finished and
/// the listener is up), or panic after ~10s.
async fn wait_for_socket(paths: &RuntimePaths) -> UnixStream {
    for _ in 0..200 {
        if let Ok(stream) = UnixStream::connect(&paths.socket_path).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "daemon socket never came up at {}",
        paths.socket_path.display()
    );
}

async fn read_frame(stream: &mut UnixStream) -> Envelope {
    tokio::time::timeout(Duration::from_secs(5), read_envelope(stream))
        .await
        .expect("read timed out")
        .expect("read frame")
        .expect("server must reply")
}

/// Handshake, asserting a `ServerHello`. A handshaken local connection defaults
/// to the `Controller` role, so no explicit attach is needed to create/control.
async fn handshake(stream: &mut UnixStream, client: ClientId) {
    let hello = ClientHello {
        client_name: "recovery-it".to_string(),
        client_version: "0".to_string(),
        supported_protocols: vec![PROTOCOL_V1],
        capabilities: ClientCapabilities::default(),
        resume_token: None,
    };
    write_envelope(
        stream,
        &Envelope::request(client, Payload::ClientHello(hello)),
    )
    .await
    .expect("write hello");
    assert!(matches!(
        read_frame(stream).await.payload,
        Payload::ServerHello(_)
    ));
}

/// Send one command and return the first non-heartbeat reply envelope.
async fn send_command(
    stream: &mut UnixStream,
    client: ClientId,
    body: CommandBody,
    key: &str,
) -> Envelope {
    let cmd = ProtoCommand {
        command_id: CommandId::new(),
        idempotency_key: key.to_string(),
        expected_revision: None,
        body,
    };
    write_envelope(stream, &Envelope::request(client, Payload::Command(cmd)))
        .await
        .expect("write command");
    loop {
        let env = read_frame(stream).await;
        if matches!(env.payload, Payload::Ping) {
            continue;
        }
        return env;
    }
}

async fn open_pool(paths: &RuntimePaths) -> SqlitePool {
    db::open_database(&paths.data_dir.join("codypendent.db"))
        .await
        .expect("open db")
}

/// Poll (against a short-lived read pool) for the single run in `session`.
async fn wait_for_run(paths: &RuntimePaths, session: SessionId) -> RunId {
    let pool = open_pool(paths).await;
    let mut found = None;
    for _ in 0..100 {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT id FROM runs WHERE session_id = ? LIMIT 1")
                .bind(session.to_string())
                .fetch_optional(&pool)
                .await
                .unwrap();
        if let Some((id,)) = row {
            found = Some(RunId::from_str(&id).unwrap());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    pool.close().await;
    found.expect("run row appeared")
}

#[tokio::test]
async fn kill9_daemon_recovers_parked_run_to_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    let paths = RuntimePaths::from_data_dir(data_dir.clone());

    // Boot the real daemon and drive it over the socket.
    let mut child = spawn_daemon(&data_dir);
    let mut stream = wait_for_socket(&paths).await;
    let client = ClientId::new();
    handshake(&mut stream, client).await;

    // Create a session (its id rides back on the envelope), start a run, then
    // pause it — `Paused` is a live state, so recovery must fail it. (The
    // executor may also fail the run first for want of a model; either path
    // leaves the run terminally `Failed`, which is what we assert.)
    let create = send_command(
        &mut stream,
        client,
        CommandBody::CreateSession {
            workspace: WorkspaceId::new(),
            title: "diagnose".to_string(),
        },
        "create",
    )
    .await;
    let session = create.session_id.expect("created session id on envelope");

    let started = send_command(
        &mut stream,
        client,
        CommandBody::StartRun {
            session_id: session,
            objective: "diagnose".to_string(),
            mode: AgentMode::Build,
        },
        "start",
    )
    .await;
    assert!(matches!(started.payload, Payload::CommandAccepted { .. }));

    let run = wait_for_run(&paths, session).await;
    let paused = send_command(
        &mut stream,
        client,
        CommandBody::PauseRun { run_id: run },
        "pause",
    )
    .await;
    assert!(matches!(paused.payload, Payload::CommandAccepted { .. }));
    drop(stream);

    // Crash the daemon uncleanly.
    let _ = child.kill();
    let _ = child.wait();

    // Restart: recovery runs before the socket reopens.
    let mut child2 = spawn_daemon(&data_dir);
    let _stream2 = wait_for_socket(&paths).await;

    // The run ended terminally `Failed`, with a RunCompleted terminal event —
    // whether by the executor's clean-fail or by restart recovery of the parked
    // projection.
    let pool = open_pool(&paths).await;
    let mut final_state = None;
    for _ in 0..100 {
        if let Some(state) = projections::load_run_state(&pool, run).await.unwrap() {
            if state == RunState::Failed {
                final_state = Some(state);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        final_state,
        Some(RunState::Failed),
        "the run must end Failed after kill -9 + restart"
    );

    let events = ledger::load_events(&pool, session).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(
            &e.body,
            EventBody::RunCompleted { run_id, disposition: RunDisposition::Failed { .. }, .. }
                if *run_id == run
        )),
        "a RunCompleted(Failed) must be recorded for the run"
    );
    pool.close().await;

    let _ = child2.kill();
    let _ = child2.wait();
}
