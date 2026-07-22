//! The Phase-5 blackboard read surface (STEP 5.3 client transport): an **Observer**
//! reading a workflow run's board over the **real** `codypendentd` socket.
//!
//! This exercises the vertical end to end — the assembly binary's wired
//! `BlackboardReader` seam, the daemon's connection-level interception of
//! `ReadBlackboard`, and the `BlackboardItemView` projection — against the actual
//! daemon process. It lives in the crate that builds the `codypendentd` binary so
//! `CARGO_BIN_EXE_codypendentd` is defined (like `docs_sync_it.rs`).
//!
//! It also pins two invariants: an **Observer may read** the board (the read
//! carries no role gate — only the executor writes it, so there is no client post
//! command to gate), and a read item's `author` is the **node identity built
//! server-side** (`{role, node_id, …}`), never the reading client — a client can
//! never appear as an author because no client post path exists.

use std::path::Path;
use std::process::{Child, Command as StdCommand, Stdio};
use std::time::Duration;

use codypendent_daemon::db;
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    read_envelope, write_envelope, BlackboardItemView, ClientCapabilities, ClientHello, ClientId,
    ClientRole, Command, CommandBody, CommandId, Envelope, Payload, SessionId, Subscription,
    WorkspaceId, PROTOCOL_V1,
};
use codypendent_workflow::{BlackboardStore, NewBlackboardItem};
use serde_json::json;
use sqlx::SqlitePool;
use tokio::net::UnixStream;

/// Owns the spawned daemon process; kills it on drop.
struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_daemon(data_dir: &Path) -> Daemon {
    let child = StdCommand::new(env!("CARGO_BIN_EXE_codypendentd"))
        .env("CODYPENDENT_DATA_DIR", data_dir)
        .env_remove("CODYPENDENT_SOCKET")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn codypendentd");
    Daemon { child }
}

async fn wait_for_socket(paths: &RuntimePaths) -> UnixStream {
    for _ in 0..200 {
        if let Ok(stream) = UnixStream::connect(&paths.socket_path).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon socket never came up");
}

async fn open_pool(paths: &RuntimePaths) -> SqlitePool {
    db::open_database(&paths.data_dir.join("codypendent.db"))
        .await
        .expect("open db")
}

/// Seed a completed workflow run (so startup recovery ignores it) holding one
/// `finding` on its board, authored as the node executor would attribute it.
/// Runs before the daemon starts, so the daemon opens a DB that already holds it.
async fn seed_board(paths: &RuntimePaths, workflow_run_id: &str) {
    paths.ensure_directories().expect("create directories");
    let pool = open_pool(paths).await;
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO workflow_runs \
         (id, workflow_id, workflow_version, graph_signature, inputs_json, state, \
          created_at, updated_at) \
         VALUES (?, 'review', 1, 'sig', 'null', 'completed', ?, ?)",
    )
    .bind(workflow_run_id)
    .bind(&now)
    .bind(&now)
    .execute(&pool)
    .await
    .expect("seed workflow run");

    BlackboardStore::new()
        .post(
            &pool,
            workflow_run_id,
            NewBlackboardItem {
                kind: codypendent_workflow::BlackboardKind::Finding,
                payload: json!({ "summary": "the parser drops trailing commas" }),
                // The author the executor builds server-side from the run context.
                author: json!({
                    "role": "investigator",
                    "node_id": "inspect",
                    "run_id": "run-xyz",
                    "workflow_run_id": workflow_run_id,
                }),
                confidence: Some(0.9),
                evidence: vec![json!({ "path": "src/parse.rs", "line": 42 })],
            },
        )
        .await
        .expect("seed finding");
    pool.close().await;
}

async fn read_frame(stream: &mut UnixStream) -> Envelope {
    tokio::time::timeout(Duration::from_secs(5), read_envelope(stream))
        .await
        .expect("read timed out")
        .expect("read frame")
        .expect("server must reply")
}

async fn send(stream: &mut UnixStream, client: ClientId, body: CommandBody, key: &str) {
    let command = Command {
        command_id: CommandId::new(),
        idempotency_key: key.to_string(),
        expected_revision: None,
        body,
    };
    write_envelope(
        stream,
        &Envelope::request(client, Payload::Command(command)),
    )
    .await
    .expect("write command");
}

async fn handshake(stream: &mut UnixStream, client: ClientId) {
    let hello = ClientHello {
        client_name: "blackboard-it".to_string(),
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

/// Create a session and return its id (rides the reply envelope).
async fn create_session(stream: &mut UnixStream, client: ClientId) -> SessionId {
    send(
        stream,
        client,
        CommandBody::CreateSession {
            workspace: WorkspaceId::new(),
            title: "bb".to_string(),
        },
        "create",
    )
    .await;
    loop {
        let env = read_frame(stream).await;
        match env.payload {
            Payload::CommandAccepted { .. } => return env.session_id.expect("session id"),
            Payload::Ping => continue,
            other => panic!("expected CommandAccepted for CreateSession, got {other:?}"),
        }
    }
}

/// Attach `stream` to `session` as an **Observer** (narrowing the connection role),
/// so a subsequent `ReadBlackboard` is issued under the Observer role.
async fn attach_as_observer(stream: &mut UnixStream, client: ClientId, session: SessionId) {
    send(
        stream,
        client,
        CommandBody::AttachSession {
            session_id: session,
            last_seen_sequence: None,
            subscriptions: vec![Subscription::SessionSummary],
            requested_role: ClientRole::Observer,
        },
        "attach",
    )
    .await;
    loop {
        match read_frame(stream).await.payload {
            Payload::Catchup { .. } => break,
            Payload::Ping => continue,
            other => panic!("expected Catchup on attach, got {other:?}"),
        }
    }
}

/// Read frames until the `BlackboardItems` reply, skipping heartbeats/events.
async fn recv_blackboard_items(stream: &mut UnixStream) -> Vec<BlackboardItemView> {
    for _ in 0..16 {
        match read_frame(stream).await.payload {
            Payload::BlackboardItems { items, .. } => return items,
            Payload::CommandRejected(error) => panic!("read rejected: {}", error.code),
            Payload::Ping | Payload::Event(_) | Payload::CommandAccepted { .. } => continue,
            other => panic!("expected BlackboardItems, got {other:?}"),
        }
    }
    panic!("no BlackboardItems arrived");
}

#[tokio::test]
async fn an_observer_reads_the_board_over_the_socket_and_sees_node_authored_items() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::from_data_dir(tmp.path().to_path_buf());
    let workflow_run_id = "wfrun-observed";
    seed_board(&paths, workflow_run_id).await;

    let _daemon = spawn_daemon(tmp.path());
    let mut stream = wait_for_socket(&paths).await;
    let client = ClientId::new();
    handshake(&mut stream, client).await;

    // Become an Observer (the read carries no role gate — an Observer may read).
    let session = create_session(&mut stream, client).await;
    attach_as_observer(&mut stream, client, session).await;

    send(
        &mut stream,
        client,
        CommandBody::ReadBlackboard {
            workflow_run_id: workflow_run_id.to_string(),
            kind: Some("finding".to_string()),
            include_superseded: false,
        },
        "read",
    )
    .await;

    let items = recv_blackboard_items(&mut stream).await;
    assert_eq!(items.len(), 1, "the seeded finding is read back");
    let item = &items[0];
    assert_eq!(item.kind, "finding");
    assert_eq!(item.workflow_run_id, workflow_run_id);
    // The author is the NODE identity the executor built server-side — never the
    // reading client. A client can never appear as an author (no post command).
    assert_eq!(
        item.author.get("node_id").and_then(|v| v.as_str()),
        Some("inspect"),
        "author is the server-built node identity, not the observer"
    );
    assert_eq!(item.confidence, Some(0.9));
}
